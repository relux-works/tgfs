//! Chunk planning: turning a resume plan into aligned sub-fetches
//! (SYNC-041).
//!
//! Backends download in blocks whatever the caller asks (SYNC-041), and a
//! fetch that ignores that alignment pays for the same block twice — once
//! for `[5, 23)` and again when the neighbouring reader asks for
//! `[0, 16)`. The planner therefore widens each remaining range to the
//! chunk grid before splitting it: the extra bytes are legal to stage
//! (staged may exceed requested, SYNC-041) and are exactly the bytes the
//! next compatible reader was about to cost.
//!
//! The rules, in order:
//!
//! 1. **Widen to the grid.** Starts round down to a chunk boundary always;
//!    ends round up only when the item's extent is known, clamped to it —
//!    with an unknown extent, a widened end past the object would turn a
//!    valid request into a source rejection.
//! 2. **Never re-fetch staged bytes.** The widened set minus the staged
//!    set is what actually goes on the wire; widening exists to help the
//!    cache, not to redo work (SYNC-046: duplicate compatible network work
//!    stays bounded).
//! 3. **Split on the grid.** Every planned sub-fetch lies within one chunk
//!    of the grid, which bounds sub-fetch size (a cancellation observed at
//!    a sub-fetch boundary is prompt at chunk granularity, SYNC-043) and
//!    keeps parallel sub-fetches block-aligned.
//!
//! Pure functions of their arguments — no clock, no entropy — so a plan is
//! exactly reproducible from the journal row it was computed from.

use gramdrive_model::ByteRange;

use crate::transfer::ranges;

/// The aligned sub-fetches that cover `remaining`, minus `staged`, in
/// offset order.
pub(crate) fn chunks(
    remaining: &[ByteRange],
    staged: &[ByteRange],
    extent: Option<u64>,
    chunk_bytes: u64,
) -> Vec<ByteRange> {
    let widened: Vec<ByteRange> = remaining
        .iter()
        .filter_map(|range| widen(*range, extent, chunk_bytes))
        .collect();
    let unstaged = ranges::subtract(&widened, staged);
    let mut out = Vec::new();
    for range in unstaged {
        split_on_grid(range, chunk_bytes, &mut out);
    }
    out
}

/// One range widened to the chunk grid, clamped to the known extent.
fn widen(range: ByteRange, extent: Option<u64>, chunk_bytes: u64) -> Option<ByteRange> {
    let start = range.start() - range.start() % chunk_bytes;
    let end = match extent {
        // `max(range.end())` keeps a range that already overruns a
        // later-discovered extent intact rather than silently shrinking
        // the demand; the source rejects it and the failure is visible.
        Some(extent) => range
            .end()
            .div_ceil(chunk_bytes)
            .saturating_mul(chunk_bytes)
            .min(extent)
            .max(range.end()),
        None => range.end(),
    };
    ByteRange::new(start, end).ok()
}

/// Splits `range` at every multiple of `chunk_bytes` it crosses.
fn split_on_grid(range: ByteRange, chunk_bytes: u64, out: &mut Vec<ByteRange>) {
    let mut cursor = range.start();
    while cursor < range.end() {
        let boundary = (cursor / chunk_bytes)
            .saturating_add(1)
            .saturating_mul(chunk_bytes);
        let end = boundary.min(range.end());
        if let Ok(piece) = ByteRange::new(cursor, end) {
            out.push(piece);
            cursor = end;
        } else {
            // Unreachable: `cursor < range.end()` and `end > cursor` by
            // construction. Bail rather than loop forever if it ever is.
            return;
        }
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
    fn a_request_widens_to_the_grid_and_splits_per_chunk() {
        assert_eq!(
            chunks(&[range(5, 23)], &[], Some(64), 16),
            vec![range(0, 16), range(16, 32)]
        );
    }

    #[test]
    fn widening_clamps_to_the_extent() {
        assert_eq!(
            chunks(&[range(50, 60)], &[], Some(60), 16),
            vec![range(48, 60)],
            "the tail chunk ends at the object, not the grid"
        );
    }

    #[test]
    fn staged_bytes_are_never_re_fetched() {
        // [0, 16) staged; demand [8, 32) widens to [0, 32) but only the
        // unstaged half goes on the wire.
        assert_eq!(
            chunks(&[range(8, 32)], &[range(0, 16)], Some(64), 16),
            vec![range(16, 32)]
        );
        // Staged bytes mid-plan split the fetch around them.
        assert_eq!(
            chunks(&[range(0, 48)], &[range(16, 32)], Some(64), 16),
            vec![range(0, 16), range(32, 48)]
        );
    }

    #[test]
    fn a_partially_staged_chunk_fetches_only_its_gap() {
        assert_eq!(
            chunks(&[range(0, 16)], &[range(0, 10)], Some(64), 16),
            vec![range(10, 16)]
        );
    }

    #[test]
    fn unknown_extent_never_widens_the_end() {
        assert_eq!(
            chunks(&[range(5, 23)], &[], None, 16),
            vec![range(0, 16), range(16, 23)],
            "the start aligns down, the end stays exactly at the demand"
        );
    }

    #[test]
    fn an_overrunning_range_is_kept_for_the_source_to_reject() {
        // Extent discovered after the request: the demand ends past it.
        // The plan keeps the overrun visible instead of silently shrinking
        // what was asked.
        assert_eq!(
            chunks(&[range(0, 80)], &[], Some(64), 16),
            vec![
                range(0, 16),
                range(16, 32),
                range(32, 48),
                range(48, 64),
                range(64, 80)
            ]
        );
    }

    #[test]
    fn multiple_ranges_stay_in_offset_order() {
        assert_eq!(
            chunks(&[range(40, 44), range(2, 6)], &[], Some(64), 16),
            vec![range(0, 16), range(32, 48)]
        );
    }

    #[test]
    fn an_empty_plan_is_empty() {
        assert_eq!(chunks(&[], &[], Some(64), 16), Vec::<ByteRange>::new());
        assert_eq!(
            chunks(&[range(0, 16)], &[range(0, 16)], Some(64), 16),
            Vec::<ByteRange>::new(),
            "fully staged demand plans nothing"
        );
    }

    #[test]
    fn huge_offsets_do_not_overflow() {
        let near_max = u64::MAX - 8;
        assert_eq!(
            chunks(&[range(near_max, u64::MAX)], &[], None, 16),
            vec![range(near_max - near_max % 16, u64::MAX)]
        );
    }
}
