//! A [`ContentSink`] that records delivery and checks it while it arrives.
//!
//! Every fetch test needs somewhere for bytes to go, and every one of them
//! would otherwise re-implement the same [`FetchProgress`] fold. Worse, a
//! test that collects bytes without folding them proves less than it looks
//! like it does: `Vec::extend` accepts an out-of-order chunk silently, so
//! the assembled buffer can be correct while the delivery that produced it
//! violated the contract.
//!
//! [`RecordingSink`] folds every chunk through [`FetchProgress`], so the
//! contract checks — contiguous from the range start, never past its end
//! (SYNC-046) — happen in every test that uses it, whether or not the test
//! thought to ask. A violation is latched into [`RecordingSink::violation`]
//! rather than panicking from inside a source's call stack: the source is
//! mid-delivery, and the useful report is the assertion the test makes
//! afterwards, not a panic unwinding through the boundary under test.
//!
//! [`stopping_after`](RecordingSink::stopping_after) covers the other
//! cancellation path — the in-band [`SinkControl::Stop`] for hosts whose
//! cancellation arrives as a callback rather than a dropped task
//! (SYNC-043).
//!
//! ```
//! # use gramdrive_testkit::RecordingSink;
//! # use gramdrive_testkit::model::ByteRange;
//! let range = ByteRange::new(0, 4).expect("valid range");
//! let mut sink = RecordingSink::new(range);
//! # assert!(!sink.is_complete());
//! # assert_eq!(sink.bytes(), b"");
//! ```

use gramdrive_model::ByteRange;
use gramdrive_source::{ContentChunk, ContentSink, DeliveryViolation, FetchProgress, SinkControl};

/// Collects delivered bytes, verifying the delivery contract as it goes.
#[derive(Debug, Clone)]
pub struct RecordingSink {
    progress: FetchProgress,
    bytes: Vec<u8>,
    chunks: Vec<ByteRange>,
    stop_after: Option<usize>,
    violation: Option<DeliveryViolation>,
}

impl RecordingSink {
    /// A sink that accepts the whole of `range`.
    pub fn new(range: ByteRange) -> Self {
        Self {
            progress: FetchProgress::new(range),
            bytes: Vec::new(),
            chunks: Vec::new(),
            stop_after: None,
            violation: None,
        }
    }

    /// A sink that returns [`SinkControl::Stop`] after accepting `chunks`
    /// chunks — in-band cancellation (SYNC-043).
    ///
    /// `0` stops on the first chunk, having still recorded it: the source
    /// had already handed the bytes over when it asked, which is precisely
    /// the state a test of "stop promptly" needs to see.
    pub fn stopping_after(range: ByteRange, chunks: usize) -> Self {
        Self {
            stop_after: Some(chunks),
            ..Self::new(range)
        }
    }

    /// The bytes delivered so far, in delivery order.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The chunk boundaries the source chose, in delivery order.
    ///
    /// The seeded chunking is reproducible, so a test may assert this
    /// exactly — that is what pins a [`ChunkPlan`](crate::ChunkPlan)
    /// against accidental change.
    pub fn chunks(&self) -> &[ByteRange] {
        &self.chunks
    }

    /// The verified accounting.
    pub fn progress(&self) -> FetchProgress {
        self.progress
    }

    /// Whether the full requested range arrived.
    pub fn is_complete(&self) -> bool {
        self.progress.is_complete()
    }

    /// The first delivery contract violation observed, if any.
    ///
    /// `Some` means the *source* is broken (SYNC-046). Nothing in this
    /// crate's own fake should ever produce one — which is exactly why the
    /// check is worth running: it is how the fake stays honest, and how the
    /// conformance suite catches a real backend that is not.
    pub fn violation(&self) -> Option<DeliveryViolation> {
        self.violation
    }
}

impl ContentSink for RecordingSink {
    fn accept(&mut self, chunk: ContentChunk<'_>) -> SinkControl {
        match self.progress.record(&chunk) {
            Ok(()) => {
                self.bytes.extend_from_slice(chunk.bytes());
                if let Ok(range) = ByteRange::new(chunk.offset(), chunk.end()) {
                    self.chunks.push(range);
                }
            }
            Err(violation) => {
                // Latch the first violation: a broken source usually keeps
                // being broken, and the first bad chunk is the diagnostic.
                // Stopping here also keeps `bytes` a faithful record of the
                // valid prefix rather than a mix of accepted and rejected
                // deliveries.
                self.violation.get_or_insert(violation);
                return SinkControl::Stop;
            }
        }

        match self.stop_after {
            Some(limit) if self.chunks.len() > limit => SinkControl::Stop,
            _ => SinkControl::Continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(start: u64, end: u64) -> ByteRange {
        ByteRange::new(start, end).unwrap()
    }

    #[test]
    fn a_contiguous_delivery_assembles_and_completes() {
        let mut sink = RecordingSink::new(range(0, 6));
        assert_eq!(
            sink.accept(ContentChunk::new(0, b"abc").unwrap()),
            SinkControl::Continue
        );
        assert_eq!(
            sink.accept(ContentChunk::new(3, b"def").unwrap()),
            SinkControl::Continue
        );

        assert_eq!(sink.bytes(), b"abcdef");
        assert!(sink.is_complete());
        assert_eq!(sink.violation(), None);
        assert_eq!(sink.progress().delivered(), 6);
        assert_eq!(sink.chunks(), &[range(0, 3), range(3, 6)]);
    }

    #[test]
    fn delivery_into_a_non_zero_range_starts_at_the_range() {
        let mut sink = RecordingSink::new(range(100, 104));
        let _ = sink.accept(ContentChunk::new(100, b"wxyz").unwrap());
        assert!(sink.is_complete());
        assert_eq!(sink.chunks(), &[range(100, 104)]);
    }

    #[test]
    fn a_gap_is_latched_as_a_violation_and_stops_delivery() {
        let mut sink = RecordingSink::new(range(0, 10));
        let _ = sink.accept(ContentChunk::new(0, b"ab").unwrap());
        let control = sink.accept(ContentChunk::new(5, b"cd").unwrap());

        assert_eq!(control, SinkControl::Stop, "a broken source is stopped");
        assert_eq!(
            sink.violation(),
            Some(DeliveryViolation::NonContiguous {
                expected_offset: 2,
                found_offset: 5
            })
        );
        assert_eq!(sink.bytes(), b"ab", "the rejected chunk is not collected");
        assert!(!sink.is_complete());
    }

    #[test]
    fn an_overrun_is_latched_as_a_violation() {
        let mut sink = RecordingSink::new(range(0, 3));
        assert_eq!(
            sink.accept(ContentChunk::new(0, b"abcd").unwrap()),
            SinkControl::Stop,
            "an overrunning source is stopped"
        );
        assert_eq!(
            sink.violation(),
            Some(DeliveryViolation::Overrun {
                range_end: 3,
                chunk_end: 4
            })
        );
        assert!(sink.bytes().is_empty());
    }

    #[test]
    fn only_the_first_violation_is_kept() {
        let mut sink = RecordingSink::new(range(0, 10));
        let _ = sink.accept(ContentChunk::new(7, b"a").unwrap());
        let _ = sink.accept(ContentChunk::new(9, b"b").unwrap());
        assert_eq!(
            sink.violation(),
            Some(DeliveryViolation::NonContiguous {
                expected_offset: 0,
                found_offset: 7
            }),
            "the first bad chunk is the diagnostic"
        );
    }

    #[test]
    fn stopping_after_a_chunk_count_asks_the_source_to_stop() {
        let mut sink = RecordingSink::stopping_after(range(0, 6), 1);
        assert_eq!(
            sink.accept(ContentChunk::new(0, b"ab").unwrap()),
            SinkControl::Continue,
            "the first chunk is within the limit"
        );
        assert_eq!(
            sink.accept(ContentChunk::new(2, b"cd").unwrap()),
            SinkControl::Stop,
            "the second exceeds it"
        );
        assert_eq!(sink.bytes(), b"abcd", "the stopping chunk was still taken");
        assert!(!sink.is_complete());
    }

    #[test]
    fn stopping_after_zero_stops_on_the_first_chunk() {
        let mut sink = RecordingSink::stopping_after(range(0, 6), 0);
        assert_eq!(
            sink.accept(ContentChunk::new(0, b"ab").unwrap()),
            SinkControl::Stop
        );
        assert_eq!(sink.bytes(), b"ab");
    }

    #[test]
    fn an_unlimited_sink_never_stops_on_its_own() {
        let mut sink = RecordingSink::new(range(0, 100));
        for offset in (0..100).step_by(10) {
            assert_eq!(
                sink.accept(ContentChunk::new(offset, &[0u8; 10]).unwrap()),
                SinkControl::Continue
            );
        }
        assert!(sink.is_complete());
    }
}
