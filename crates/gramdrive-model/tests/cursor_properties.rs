//! Property suite for change-cursor serialization (DOM-004, SYNC-004).
//!
//! Proves over sampled input what the unit tests show by example: every
//! cursor round-trips through its text form unchanged, distinct cursors
//! never share a text form, parsing is total (no input panics), and every
//! text the parser accepts is canonical — re-encoding a decoded cursor
//! reproduces the input byte-for-byte, so each cursor has exactly one
//! spelling. The format carries no checksum on purpose (the payload is
//! opaque provider state, like `ItemId` fields); corruption *detection* is
//! not a cursor guarantee — scope and schema rejection are.

use gramdrive_model::cursor::{ChangeCursor, MAX_CURSOR_PAYLOAD_BYTES};
use gramdrive_model::identity::{AccountId, AccountKey, AccountScope, NamespaceVersion};
use proptest::prelude::*;

fn scope_strategy() -> impl Strategy<Value = AccountScope> {
    (any::<i64>(), any::<u32>()).prop_map(|(account, namespace)| AccountScope {
        account: AccountKey {
            account_id: AccountId(account),
        },
        namespace_version: NamespaceVersion(namespace),
    })
}

fn cursor_strategy() -> impl Strategy<Value = ChangeCursor> {
    (
        scope_strategy(),
        proptest::collection::vec(any::<u8>(), 0..128),
    )
        // Payloads are sampled far below the cap, so the filter never
        // actually rejects; filter_map keeps this fixture free of unwrap
        // outside a #[test] body (the workspace denies expect/unwrap and
        // clippy's test exemption is lexical).
        .prop_filter_map("payload within cap", |(scope, payload)| {
            ChangeCursor::new(scope, payload).ok()
        })
}

proptest! {
    #[test]
    fn round_trips_through_text(cursor in cursor_strategy()) {
        let decoded = ChangeCursor::decode(&cursor.encode()).expect("own encoding decodes");
        prop_assert_eq!(decoded, cursor);
    }

    #[test]
    fn distinct_cursors_have_distinct_text(a in cursor_strategy(), b in cursor_strategy()) {
        if a != b {
            prop_assert_ne!(a.encode(), b.encode());
        }
    }

    #[test]
    fn parsing_is_total(text in ".*") {
        // Arbitrary input, including non-ASCII and embedded NULs, must be
        // answered with Ok or a structured error — never a panic.
        let _ = ChangeCursor::decode(&text);
    }

    #[test]
    fn accepted_text_is_canonical(payload in "[a-z2-7]{0,64}") {
        // Every string over the alphabet that the strict parser accepts
        // must be the one spelling its cursor encodes back to.
        let text = format!("gdc-{payload}");
        if let Ok(cursor) = ChangeCursor::decode(&text) {
            prop_assert_eq!(cursor.encode(), text);
        }
    }

    #[test]
    fn payload_cap_is_enforced(
        scope in scope_strategy(),
        extra in 1usize..64,
    ) {
        let oversized = vec![0u8; MAX_CURSOR_PAYLOAD_BYTES + extra];
        prop_assert!(ChangeCursor::new(scope, oversized).is_err());
    }
}
