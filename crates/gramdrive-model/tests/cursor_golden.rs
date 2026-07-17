//! Golden fixtures freezing change-cursor serialization format v1.
//!
//! Same policy as the identity goldens (`identity_golden.rs`): cursors are
//! durable (SYNC-004) — a cursor written today must decode in every future
//! app version, so v1 is frozen by these literals. If a change breaks one of
//! them, the change is wrong; the correct evolution is a new format version
//! byte decoded alongside v1, never a mutation of v1.
//!
//! The expected strings were computed independently of the implementation
//! (RFC 4648 base32 of the documented byte layout).

use gramdrive_model::cursor::ChangeCursor;
use gramdrive_model::identity::{AccountId, AccountKey, AccountScope, NamespaceVersion};

fn scope(account: i64, namespace: u32) -> AccountScope {
    AccountScope {
        account: AccountKey {
            account_id: AccountId(account),
        },
        namespace_version: NamespaceVersion(namespace),
    }
}

#[test]
fn v1_encoding_is_frozen() {
    let cases: [(AccountScope, &[u8], &str); 2] = [
        (
            scope(42, 7),
            b"pts:12345",
            "gdc-aeaaaaaaaaaaakqaaaaao4duom5dcmrtgq2q",
        ),
        // Negative account id (two's complement) and empty payload.
        (scope(-1, 0), b"", "gdc-ah777777777777yaaaaaa"),
    ];
    for (scope, payload, expected) in cases {
        let cursor = ChangeCursor::new(scope, payload.to_vec()).expect("payload within cap");
        assert_eq!(cursor.encode(), expected);
        let decoded = ChangeCursor::decode(expected).expect("golden text must decode");
        assert_eq!(decoded, cursor);
    }
}

#[test]
fn golden_text_survives_scope_gate() {
    let decoded =
        ChangeCursor::decode("gdc-aeaaaaaaaaaaakqaaaaao4duom5dcmrtgq2q").expect("golden decodes");
    assert!(decoded.require_scope(scope(42, 7)).is_ok());
    assert!(decoded.require_scope(scope(42, 8)).is_err());
}
