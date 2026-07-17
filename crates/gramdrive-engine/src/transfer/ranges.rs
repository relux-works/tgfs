//! Range-set arithmetic over [`ByteRange`] lists.
//!
//! The transfer machine reasons about *sets* of bytes — what was requested,
//! what is durably staged, what remains, whether one covers the other — but
//! the journal stores plain lists in whatever shape the caller produced
//! (SYNC-041 lets a source stage more than was asked; SYNC-046 lets several
//! readers contribute overlapping demand). These functions are the one place
//! that turns lists into canonical sets, so every gate in the machine
//! compares like with like.
//!
//! Every function accepts arbitrary lists — overlapping, adjacent, unsorted —
//! and normalizes internally. The lists involved are a handful of entries;
//! re-normalizing per call buys simplicity at no measurable cost.

use gramdrive_model::ByteRange;

/// Canonical form: sorted by start, no overlapping or adjacent ranges.
///
/// Adjacent ranges merge because `[0, 5)` + `[5, 10)` describe the same
/// bytes as `[0, 10)` — a coverage check that treated them as different
/// would refuse to promote complete content.
pub(crate) fn normalize(ranges: &[ByteRange]) -> Vec<ByteRange> {
    let mut sorted: Vec<ByteRange> = ranges.to_vec();
    sorted.sort_by_key(ByteRange::start);
    let mut out: Vec<ByteRange> = Vec::with_capacity(sorted.len());
    for range in sorted {
        match out.last_mut() {
            Some(last) if range.start() <= last.end() => {
                if range.end() > last.end()
                    && let Ok(merged) = ByteRange::new(last.start(), range.end())
                {
                    *last = merged;
                }
            }
            _ => out.push(range),
        }
    }
    out
}

/// The bytes of `target` not covered by `cover`, in canonical form.
///
/// This is both the resume plan (requested minus staged, SYNC-042) and the
/// promotion gate's evidence (an empty answer is what "complete" means).
pub(crate) fn subtract(target: &[ByteRange], cover: &[ByteRange]) -> Vec<ByteRange> {
    let target = normalize(target);
    let cover = normalize(cover);
    let mut out = Vec::new();
    // Both lists are sorted and disjoint; `first_relevant` only ever moves
    // past cover ranges that end before everything still to come.
    let mut first_relevant = 0;
    for want in target {
        let mut start = want.start();
        while first_relevant < cover.len() && cover[first_relevant].end() <= start {
            first_relevant += 1;
        }
        for held in &cover[first_relevant..] {
            if held.start() >= want.end() {
                break;
            }
            if held.start() > start {
                push_gap(&mut out, start, held.start());
            }
            start = start.max(held.end());
            if start >= want.end() {
                break;
            }
        }
        if start < want.end() {
            push_gap(&mut out, start, want.end());
        }
    }
    out
}

/// Whether `cover` contains every byte of `target`.
pub(crate) fn covers(cover: &[ByteRange], target: &[ByteRange]) -> bool {
    subtract(target, cover).is_empty()
}

/// The whole object `[0, size)` as a range list; empty for a zero-size
/// object, which is a complete object with no bytes to fetch.
pub(crate) fn whole_object(size: u64) -> Vec<ByteRange> {
    ByteRange::new(0, size).into_iter().collect()
}

/// Pushes `[start, end)` when it is non-empty. The guard is what lets the
/// arithmetic above stay free of unwraps: every gap it produces satisfies
/// the `ByteRange` invariant by construction, and a gap that does not is
/// simply not a gap.
fn push_gap(out: &mut Vec<ByteRange>, start: u64, end: u64) {
    if let Ok(range) = ByteRange::new(start, end) {
        out.push(range);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    fn range(start: u64, end: u64) -> ByteRange {
        ByteRange::new(start, end).expect("valid range")
    }

    #[test]
    fn normalize_sorts_and_merges_overlap_and_adjacency() {
        assert_eq!(normalize(&[]), vec![]);
        assert_eq!(
            normalize(&[range(10, 20), range(0, 5)]),
            vec![range(0, 5), range(10, 20)]
        );
        // Overlap, adjacency, and containment all collapse.
        assert_eq!(
            normalize(&[range(0, 5), range(5, 10), range(8, 12), range(9, 11)]),
            vec![range(0, 12)]
        );
        assert_eq!(
            normalize(&[range(3, 4), range(0, 100)]),
            vec![range(0, 100)]
        );
    }

    #[test]
    fn subtract_reports_exactly_the_uncovered_bytes() {
        // Nothing covered: the target comes back canonical.
        assert_eq!(
            subtract(&[range(10, 20), range(0, 5)], &[]),
            vec![range(0, 5), range(10, 20)]
        );
        // Cover splits a target range.
        assert_eq!(
            subtract(&[range(0, 64)], &[range(16, 32)]),
            vec![range(0, 16), range(32, 64)]
        );
        // One cover range spans several target ranges.
        assert_eq!(
            subtract(
                &[range(0, 10), range(20, 30), range(40, 50)],
                &[range(5, 45)]
            ),
            vec![range(0, 5), range(45, 50)]
        );
        // Fully covered, including by an assembled cover.
        assert_eq!(
            subtract(&[range(0, 64)], &[range(32, 64), range(0, 32)]),
            vec![]
        );
        // Cover outside the target changes nothing.
        assert_eq!(
            subtract(&[range(10, 20)], &[range(0, 10), range(20, 30)]),
            vec![range(10, 20)]
        );
    }

    #[test]
    fn covers_is_subset_containment() {
        assert!(covers(&[range(0, 64)], &[range(10, 20)]));
        assert!(covers(&[range(0, 32), range(32, 64)], &[range(0, 64)]));
        assert!(!covers(&[range(0, 32)], &[range(0, 33)]));
        assert!(!covers(&[], &[range(0, 1)]));
        // The empty target is covered by anything, including nothing.
        assert!(covers(&[], &[]));
    }

    #[test]
    fn whole_object_of_zero_size_is_empty() {
        assert_eq!(whole_object(0), vec![]);
        assert_eq!(whole_object(64), vec![range(0, 64)]);
    }
}
