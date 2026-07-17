//! The per-sub-fetch sink and the delivery state it shares with the
//! coordinator.
//!
//! Each parallel sub-fetch owns one [`ChunkSink`]; every sink holds the
//! same [`SharedDelivery`] behind a mutex. The sink does three things per
//! chunk, in order: verify the delivery contract through
//! [`FetchProgress`] (a source that delivers a gap, an overlap, or an
//! overrun is caught at the first bad chunk, SYNC-046), write the bytes
//! into staging at their absolute offset, and record the written span so
//! the coordinator can fold it into durable progress at the next work
//! boundary.
//!
//! Anything that goes wrong is *latched*, not thrown: the sink answers
//! [`SinkControl::Stop`], the source resolves the fetch with
//! `Cancelled`, and the coordinator reads the latch to learn the real
//! fault. A panic here would unwind through the source implementation
//! mid-delivery (NFR-030 forbids exactly that), and an error return does
//! not exist in the [`ContentSink`] contract — the latch is the channel.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use gramdrive_model::ByteRange;
use gramdrive_source::{ContentChunk, ContentSink, DeliveryViolation, FetchProgress, SinkControl};

use super::staging::{Staging, StagingError};

/// Locks recovering from poison: the workspace denies panics, so a
/// poisoned lock means a panic already escaped somewhere else; the state
/// under the mutex is a log of written ranges and a latch, both of which
/// remain meaningful.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Why an attempt must stop, observed inside a sink.
#[derive(Debug)]
pub(crate) enum Breakage {
    /// The source violated the delivery contract (SYNC-046): the transfer
    /// fails rather than accounting untrustworthy bytes.
    Violation(DeliveryViolation),
    /// Staging refused the bytes; classified per [`StagingError`].
    Staging(StagingError),
}

/// Delivery state shared by every sub-fetch of one attempt.
#[derive(Debug, Default)]
pub(crate) struct SharedDelivery {
    /// The open staging area; `None` only for attempts with nothing to
    /// write or read.
    staging: Option<Box<dyn Staging>>,
    /// Spans written to staging since the coordinator last drained them.
    written: Vec<ByteRange>,
    /// Raised by the coordinator to stop every in-flight sub-fetch
    /// in-band (SYNC-043); dropping the futures is the other path.
    stop: bool,
    /// The first observed breakage; later ones add nothing.
    breakage: Option<Breakage>,
}

impl SharedDelivery {
    /// Installs the open staging area for this attempt.
    pub(crate) fn set_staging(&mut self, staging: Box<dyn Staging>) {
        self.staging = Some(staging);
    }

    /// Raises the in-band stop flag (SYNC-043).
    pub(crate) fn request_stop(&mut self) {
        self.stop = true;
    }

    /// Takes the spans written since the last drain.
    pub(crate) fn take_written(&mut self) -> Vec<ByteRange> {
        std::mem::take(&mut self.written)
    }

    /// Takes the latched breakage, if any.
    pub(crate) fn take_breakage(&mut self) -> Option<Breakage> {
        self.breakage.take()
    }

    /// Reads previously written bytes for reader streaming.
    pub(crate) fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), StagingError> {
        match &self.staging {
            Some(staging) => staging.read_at(offset, buf),
            None => Err(StagingError::Failed {
                detail: "no staging area is open for this attempt".to_owned(),
            }),
        }
    }

    fn write_chunk(&mut self, chunk: &ContentChunk<'_>) -> Result<(), StagingError> {
        let Some(staging) = self.staging.as_mut() else {
            return Err(StagingError::Failed {
                detail: "no staging area is open for this attempt".to_owned(),
            });
        };
        staging.write_at(chunk.offset(), chunk.bytes())
    }
}

/// The [`ContentSink`] one sub-fetch delivers into.
#[derive(Debug)]
pub(crate) struct ChunkSink {
    shared: Arc<Mutex<SharedDelivery>>,
    progress: FetchProgress,
}

impl ChunkSink {
    /// A sink accepting exactly `chunk` of the content object.
    pub(crate) fn new(chunk: ByteRange, shared: Arc<Mutex<SharedDelivery>>) -> Self {
        Self {
            shared,
            progress: FetchProgress::new(chunk),
        }
    }

    /// Bytes delivered and staged so far, contiguous from the sub-fetch's
    /// start.
    pub(crate) fn delivered(&self) -> u64 {
        self.progress.delivered()
    }

    /// Whether the sub-fetch's whole range arrived.
    pub(crate) fn is_complete(&self) -> bool {
        self.progress.is_complete()
    }
}

impl ContentSink for ChunkSink {
    fn accept(&mut self, chunk: ContentChunk<'_>) -> SinkControl {
        // Verify before writing: a violating chunk never reaches staging
        // and is never accounted (SYNC-046).
        if let Err(violation) = self.progress.record(&chunk) {
            let mut shared = lock(&self.shared);
            shared
                .breakage
                .get_or_insert(Breakage::Violation(violation));
            return SinkControl::Stop;
        }
        let mut shared = lock(&self.shared);
        if shared.stop {
            // The bytes were already handed over; refusing to write them
            // costs nothing and the attempt is ending anyway.
            return SinkControl::Stop;
        }
        match shared.write_chunk(&chunk) {
            Ok(()) => {
                if let Ok(span) = ByteRange::new(chunk.offset(), chunk.end()) {
                    shared.written.push(span);
                }
                SinkControl::Continue
            }
            Err(error) => {
                shared.breakage.get_or_insert(Breakage::Staging(error));
                SinkControl::Stop
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    /// In-memory staging for the unit tests below; the integration suite
    /// carries its own host-side implementation.
    #[derive(Debug, Default)]
    struct MemoryStaging {
        bytes: Vec<u8>,
        fail_writes: bool,
    }

    impl Staging for MemoryStaging {
        fn handle(&self) -> &str {
            "memory"
        }

        fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<(), StagingError> {
            if self.fail_writes {
                return Err(StagingError::Full {
                    detail: "scripted".to_owned(),
                });
            }
            let offset = usize::try_from(offset).expect("test offsets fit usize");
            let end = offset + bytes.len();
            if self.bytes.len() < end {
                self.bytes.resize(end, 0);
            }
            self.bytes[offset..end].copy_from_slice(bytes);
            Ok(())
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), StagingError> {
            let offset = usize::try_from(offset).expect("test offsets fit usize");
            let end = offset + buf.len();
            let Some(slice) = self.bytes.get(offset..end) else {
                return Err(StagingError::Failed {
                    detail: "read past written bytes".to_owned(),
                });
            };
            buf.copy_from_slice(slice);
            Ok(())
        }
    }

    fn shared_with(staging: MemoryStaging) -> Arc<Mutex<SharedDelivery>> {
        let shared = Arc::new(Mutex::new(SharedDelivery::default()));
        lock(&shared).set_staging(Box::new(staging));
        shared
    }

    fn range(start: u64, end: u64) -> ByteRange {
        ByteRange::new(start, end).expect("valid range")
    }

    #[test]
    fn accepted_chunks_are_staged_and_recorded() {
        let shared = shared_with(MemoryStaging::default());
        let mut sink = ChunkSink::new(range(16, 24), Arc::clone(&shared));

        assert_eq!(
            sink.accept(ContentChunk::new(16, b"abcd").expect("chunk")),
            SinkControl::Continue
        );
        assert_eq!(
            sink.accept(ContentChunk::new(20, b"efgh").expect("chunk")),
            SinkControl::Continue
        );
        assert!(sink.is_complete());
        assert_eq!(sink.delivered(), 8);

        let mut guard = lock(&shared);
        assert_eq!(guard.take_written(), vec![range(16, 20), range(20, 24)]);
        let mut buf = [0u8; 8];
        guard.read_at(16, &mut buf).expect("readable");
        assert_eq!(&buf, b"abcdefgh");
        assert!(guard.take_breakage().is_none());
    }

    #[test]
    fn a_contract_violation_latches_and_stops_before_staging() {
        let shared = shared_with(MemoryStaging::default());
        let mut sink = ChunkSink::new(range(0, 8), Arc::clone(&shared));

        assert_eq!(
            sink.accept(ContentChunk::new(4, b"zz").expect("chunk")),
            SinkControl::Stop,
            "a gap at the start is a violation"
        );
        let mut guard = lock(&shared);
        assert!(matches!(
            guard.take_breakage(),
            Some(Breakage::Violation(DeliveryViolation::NonContiguous { .. }))
        ));
        assert!(guard.take_written().is_empty(), "nothing was staged");
    }

    #[test]
    fn a_failed_write_latches_its_classification() {
        let shared = shared_with(MemoryStaging {
            fail_writes: true,
            ..MemoryStaging::default()
        });
        let mut sink = ChunkSink::new(range(0, 8), Arc::clone(&shared));

        assert_eq!(
            sink.accept(ContentChunk::new(0, b"ab").expect("chunk")),
            SinkControl::Stop
        );
        assert!(matches!(
            lock(&shared).take_breakage(),
            Some(Breakage::Staging(StagingError::Full { .. }))
        ));
    }

    #[test]
    fn the_stop_flag_stops_the_next_chunk_in_band() {
        let shared = shared_with(MemoryStaging::default());
        let mut sink = ChunkSink::new(range(0, 8), Arc::clone(&shared));
        assert_eq!(
            sink.accept(ContentChunk::new(0, b"ab").expect("chunk")),
            SinkControl::Continue
        );
        lock(&shared).request_stop();
        assert_eq!(
            sink.accept(ContentChunk::new(2, b"cd").expect("chunk")),
            SinkControl::Stop,
            "the flag is observed at the very next chunk (SYNC-043)"
        );
        assert!(
            lock(&shared).take_breakage().is_none(),
            "a requested stop is not a breakage"
        );
    }
}
