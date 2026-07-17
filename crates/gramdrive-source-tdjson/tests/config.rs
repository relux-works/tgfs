//! Configuration and storage-policy fixtures (TASK-260715-1hdnuy):
//! secrets never reach a log, the storage identity survives a version
//! upgrade, logout wipes one account's on-disk state cleanly while leaving
//! siblings intact, and the startup request sequence round-trips through the
//! real runtime over the deterministic mock.

// clippy.toml exempts test code; restated for the module-level bodies of
// this integration binary (matching the established test-suite pattern).
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use common::{GUARD, echo_ok_responder, start_runtime, test_config};
use gramdrive_model::identity::AccountId;
use gramdrive_source_tdjson::config::{
    AccountConfig, ApiCredentials, DatabaseKey, DeviceMetadata, InMemorySecrets, Proxy, Secret,
    StorageLayout, TdlibConfig,
};

// Sentinels chosen so any leak into a rendered string is unmistakable.
const API_HASH_SENTINEL: &str = "APIHASH-11111111-leak-me";
const DB_KEY_SENTINEL: &[u8] = b"DBKEY-22222222-leak-me";
const PROXY_PW_SENTINEL: &str = "PROXYPW-33333333-leak-me";
const API_ID_SENTINEL: i32 = 987654;

/// A unique-per-call temp directory, using the process id and a counter so
/// parallel test binaries cannot collide (no clock, no randomness) —
/// matching the established fixture pattern in the state crate.
fn temp_root() -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "gramdrive-tdlib-config-test-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    path
}

fn secrets() -> InMemorySecrets {
    InMemorySecrets::new(ApiCredentials {
        api_id: API_ID_SENTINEL,
        api_hash: Secret::new(API_HASH_SENTINEL),
    })
    .with_key(
        AccountId(7),
        DatabaseKey::from_bytes(DB_KEY_SENTINEL.to_vec()),
    )
}

/// A resolved config using a proxy, so the proxy password is in the log-scrub
/// surface too.
fn resolved_with_proxy(layout: &StorageLayout) -> TdlibConfig {
    let mut plan = AccountConfig::mirror(AccountId(7), layout);
    plan.proxy = Proxy::Socks5 {
        server: "127.0.0.1".to_owned(),
        port: 1080,
        username: Some("user".to_owned()),
        password: Some(Secret::new(PROXY_PW_SENTINEL)),
    };
    plan.resolve(&secrets()).expect("secrets resolve")
}

#[test]
fn no_secret_reaches_any_debug_or_log_form() {
    let layout = StorageLayout::new("/root");
    let config = resolved_with_proxy(&layout);

    // The resolved config, its loggable plan, and the plan's proxy are the
    // three renderings anything diagnostic would reach for.
    let surfaces = [
        format!("{config:?}"),
        format!("{:?}", config.plan()),
        format!("{:?}", config.plan().proxy),
    ];
    for rendered in &surfaces {
        for needle in [API_HASH_SENTINEL, PROXY_PW_SENTINEL] {
            assert!(
                !rendered.contains(needle),
                "secret leaked into a Debug form: {rendered}"
            );
        }
        assert!(
            !rendered.contains(&API_ID_SENTINEL.to_string()),
            "api_id leaked into a Debug form: {rendered}"
        );
        assert!(
            !rendered.contains(std::str::from_utf8(DB_KEY_SENTINEL).unwrap()),
            "database key leaked into a Debug form: {rendered}"
        );
    }

    // The wire request, by contrast, MUST carry the secrets — it goes to
    // TDLib, not to a log. This asserts the split is real, not that the
    // builder dropped the values.
    let request = config.set_parameters_request().to_string();
    assert!(request.contains(API_HASH_SENTINEL));
}

#[test]
fn storage_identity_survives_a_version_upgrade() {
    let layout = StorageLayout::new("/root");

    let mut before = AccountConfig::mirror(AccountId(7), &layout);
    before.device = DeviceMetadata {
        application_version: "1.0.0".to_owned(),
        ..DeviceMetadata::default()
    };
    let before = before.resolve(&secrets()).unwrap().set_parameters_request();

    let mut after = AccountConfig::mirror(AccountId(7), &layout);
    after.device = DeviceMetadata {
        application_version: "2.5.1".to_owned(),
        ..DeviceMetadata::default()
    };
    let after = after.resolve(&secrets()).unwrap().set_parameters_request();

    // The version changed...
    assert_eq!(before["application_version"], "1.0.0");
    assert_eq!(after["application_version"], "2.5.1");
    // ...but every field that decides which encrypted database TDLib opens is
    // byte-identical, so the upgrade reopens the same store rather than
    // starting a fresh login.
    for field in [
        "database_directory",
        "files_directory",
        "database_encryption_key",
        "use_test_dc",
        "use_file_database",
        "use_chat_info_database",
        "use_message_database",
        "use_secret_chats",
        "api_id",
        "api_hash",
    ] {
        assert_eq!(
            before[field], after[field],
            "field '{field}' changed on upgrade"
        );
    }
}

#[test]
fn logout_wipes_one_account_cleanly_and_leaves_siblings_intact() {
    let root = temp_root();
    let layout = StorageLayout::new(&root);
    let victim = AccountId(7);
    let bystander = AccountId(8);

    // Materialize both accounts' subtrees with real files, as TDLib would.
    for account in [victim, bystander] {
        let paths = layout.account_paths(account);
        std::fs::create_dir_all(paths.database_directory()).unwrap();
        std::fs::create_dir_all(paths.files_directory()).unwrap();
        std::fs::write(paths.database_directory().join("db.binlog"), b"state").unwrap();
        std::fs::write(paths.files_directory().join("blob.bin"), b"content").unwrap();
    }
    assert!(layout.account_dir(victim).exists());
    assert!(layout.account_dir(bystander).exists());

    // Logout wipes exactly the victim's subtree...
    layout.wipe_account(victim).expect("wipe succeeds");
    assert!(!layout.account_dir(victim).exists());
    // ...and the sibling is untouched (isolation).
    assert!(layout.account_dir(bystander).exists());
    assert!(
        layout
            .account_paths(bystander)
            .files_directory()
            .join("blob.bin")
            .exists()
    );

    // Wiping again is a no-op, not an error: logout must converge.
    layout
        .wipe_account(victim)
        .expect("second wipe is idempotent");

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn startup_sequence_round_trips_through_the_runtime() {
    // Proves the built requests are well-formed JSON the runtime accepts,
    // correlates, and answers — the shape the authorization flow will send.
    let layout = StorageLayout::new("/root");
    let config = resolved_with_proxy(&layout);
    let requests = config.startup_requests();
    // params + five memory options + one proxy.
    assert_eq!(requests.len(), 7);

    let (runtime, mock) = start_runtime(test_config());
    mock.set_responder(echo_ok_responder());
    let (client, _updates) = runtime.create_client().unwrap();

    let pending: Vec<_> = requests
        .into_iter()
        .map(|request| client.request(request).expect("request accepted"))
        .collect();
    for handle in pending {
        let answer = handle.wait_timeout(GUARD).expect("resolves").unwrap();
        assert_eq!(answer["@type"], "ok");
    }

    // Every request carried a runtime-minted @extra and a recognizable type;
    // the first is setTdlibParameters, the last addProxy.
    let sent = mock.take_sent();
    assert_eq!(sent.len(), 7);
    assert_eq!(sent[0].request_type().unwrap(), "setTdlibParameters");
    assert_eq!(sent[6].request_type().unwrap(), "addProxy");
    for request in &sent {
        assert!(request.extra().is_some(), "runtime injects @extra");
    }
}
