//! Durable pin orchestration (TASK-260715-11abx8; POL-2, SYNC-051).
//! Module-level rationale is in [`super`].

use gramdrive_model::identity::ItemId;
use gramdrive_state::StateStore;
use gramdrive_state::repo::PinOrigin;

use crate::transfer::EngineError;

/// What a [`pin`] resolved to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinOutcome {
    /// The pin origin now in effect. A user pin over Archive-Mode coverage
    /// resolves to [`PinOrigin::User`] — the explicit intent wins and
    /// survives Archive Mode turning off (POL-2).
    pub origin: PinOrigin,
    /// Whether the durable pin row was created or changed. `false` means the
    /// item was already pinned at an origin this call would not downgrade.
    pub changed: bool,
    /// Whether a materialized cache entry existed and took the folded pin
    /// flag, so the eviction scan sees it as protected without a join.
    pub folded: bool,
}

/// Records durable offline intent for `item` and folds it onto the
/// materialized cache entry, in one transaction (POL-2, SYNC-051).
///
/// Origin is *directional*: an explicit [`PinOrigin::User`] pin overwrites
/// Archive-Mode coverage (a deliberate upgrade), but Archive-Mode coverage
/// never downgrades an existing user pin. This is the one place the blind
/// origin overwrite of [`WriteTxn::pin_item`](gramdrive_state::WriteTxn::pin_item)
/// is resolved, so the two callers — the user action and the Archive-Mode
/// scope walk — cannot clobber each other.
///
/// The item must already be projected; folding is a no-op when the content
/// is not yet materialized (promotion will fold the pin itself when it lands,
/// so a pin set before hydration still protects the eventual bytes).
pub fn pin(
    store: &mut StateStore,
    item: &ItemId,
    origin: PinOrigin,
    now_ms: i64,
) -> Result<PinOutcome, EngineError> {
    let tx = store.write_txn()?;
    let existing = tx.read().pin(item)?.map(|record| record.origin);
    // User intent wins; Archive-Mode coverage does not downgrade it.
    let resolved = match (existing, origin) {
        (Some(PinOrigin::User), PinOrigin::ArchiveMode) => PinOrigin::User,
        _ => origin,
    };
    let changed = existing != Some(resolved);
    if changed {
        tx.pin_item(item, resolved, now_ms)?;
    }
    let folded = if tx.read().cache_entry(item)?.is_some() {
        tx.set_cache_pin(item, Some(resolved))?;
        true
    } else {
        false
    };
    tx.commit()?;
    Ok(PinOutcome {
        origin: resolved,
        changed,
        folded,
    })
}

/// Releases the pin on `item` and clears the folded flag, in one transaction
/// (POL-2, SYNC-062).
///
/// Directional, mirroring [`pin`]: the release applies only to a pin of the
/// given `origin`. Archive-Mode teardown (`origin = ArchiveMode`) frees
/// exactly its own coverage and leaves an explicit user pin standing; a user
/// unpin (`origin = User`) frees only a user pin and never a bare
/// Archive-Mode cover. Returns whether a pin was released.
pub fn unpin(
    store: &mut StateStore,
    item: &ItemId,
    origin: PinOrigin,
) -> Result<bool, EngineError> {
    let tx = store.write_txn()?;
    if tx.read().pin(item)?.map(|record| record.origin) != Some(origin) {
        // Nothing of this origin to release; leave any other-origin pin intact.
        drop(tx);
        return Ok(false);
    }
    tx.unpin_item(item)?;
    if tx.read().cache_entry(item)?.is_some() {
        tx.set_cache_pin(item, None)?;
    }
    tx.commit()?;
    Ok(true)
}
