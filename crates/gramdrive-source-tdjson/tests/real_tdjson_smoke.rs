//! Real-linkage smoke: the same runtime the mock tests exercise, driven
//! against the staged `libtdjson.dylib` (TASK-260715-2ulon7).
//!
//! Compiled only under `cfg(real_tdjson)` — the env gate `build.rs` turns
//! on when `GRAMDRIVE_TDLIB_ARTIFACT_DIR` points at the artifact — so this
//! file is an empty test binary in every gate run and a linked one under
//! `make tdjson-smoke`. Like the tdlib link smoke, it needs no api_id and
//! no network: `getOption "version"` is answered before authorization.
#![cfg(real_tdjson)]
// clippy.toml exempts test code; restated for the module-level test bodies
// of this integration binary (matching the established test-suite pattern).
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::time::Duration;

use gramdrive_model::ByteRange;
use gramdrive_model::version::ContentVersion;
use gramdrive_source::FetchRequest;
use gramdrive_source_tdjson::message::AttachmentAvailability;
use gramdrive_source_tdjson::real::RealTdJson;
use gramdrive_source_tdjson::{
    CatalogEntry, DownloadConfig, DownloadMachine, DownloadStep, FileTarget, RuntimeConfig,
    TdError, TdRuntime, UpdateRecvError,
};
use serde_json::{Value, json};

/// Every request shape `download.rs` can put on the wire — the ranged
/// download, the reference refresh, and the cancel — built by the machine
/// itself, so the probe cannot drift from the adapter.
fn download_adapter_payloads() -> Vec<Value> {
    use gramdrive_model::identity::{
        AccountId, AccountKey, AccountScope, AttachmentIndex, AttachmentKey, CanonicalKey, ChatId,
        ChatKey, ItemKey, MessageId, MessageKey, NamespaceVersion,
    };
    let item = ItemKey::Canonical(CanonicalKey::Attachment(AttachmentKey {
        message: MessageKey {
            chat: ChatKey {
                scope: AccountScope {
                    account: AccountKey {
                        account_id: AccountId(1),
                    },
                    namespace_version: NamespaceVersion(1),
                },
                chat_id: ChatId(1),
            },
            message_id: MessageId(1),
        },
        index: AttachmentIndex(0),
    }))
    .id();
    let request = FetchRequest {
        item,
        version: ContentVersion::new("c1").expect("valid token"),
        range: ByteRange::new(0, 64).expect("valid range"),
    };
    let target = FileTarget {
        file_id: 1,
        remote_id: None,
        remote_file_type: None,
        refresh: gramdrive_source_tdjson::download::RefreshTarget::Message {
            chat_id: 1,
            message_id: 1,
        },
        availability: AttachmentAvailability::Fetchable,
        remote_unique_id: None,
        size: None,
        version: ContentVersion::new("c1").expect("valid token"),
    };
    let mut machine = DownloadMachine::new(
        &request,
        Some(CatalogEntry::File(target)),
        &DownloadConfig::default(),
    );
    let mut payloads = Vec::new();
    let Ok(DownloadStep::Submit { payload, .. }) = machine.next_step() else {
        panic!("the first obligation is the download");
    };
    payloads.push(payload);
    payloads.extend(machine.cancel_request());
    // A stale-reference rejection turns the machine to the refresh request.
    machine
        .on_response(Err(TdError::Td {
            code: 400,
            message: "FILE_REFERENCE_EXPIRED".to_owned(),
        }))
        .expect("a stale reference arms the refresh");
    let Ok(DownloadStep::Submit { payload, .. }) = machine.next_step() else {
        panic!("the refresh is a request obligation");
    };
    payloads.push(payload);
    payloads
}

/// TDLib answers the version probe from local state, but its client thread
/// has to start first; generous so slow CI cannot flake this.
const REAL_GUARD: Duration = Duration::from_secs(30);

#[test]
fn runtime_over_real_tdjson_correlates_and_shuts_down() {
    let (sender, receiver) = RealTdJson::claim().expect("first claim gets the receiver");
    assert!(
        RealTdJson::claim().is_none(),
        "the receive stream is single-owner"
    );

    let runtime =
        TdRuntime::start(sender, receiver, RuntimeConfig::default()).expect("runtime starts");

    // td_execute round-trip (and quiet logging for the smoke output).
    let answer = runtime
        .execute(&json!({"@type": "setLogVerbosityLevel", "new_verbosity_level": 1}))
        .expect("execute answers");
    assert_eq!(answer["@type"], "ok");

    // One correlated request against a live client: the version comes out
    // of the running library, through the runtime's @extra correlation.
    let (client, updates) = runtime.create_client().expect("client registers");
    let pending = client
        .request(json!({"@type": "getOption", "name": "version"}))
        .expect("request submits");
    let request_id = pending.request_id();
    let version = pending
        .wait_timeout(REAL_GUARD)
        .expect("TDLib answered the version probe")
        .expect("probe answer is not an error");
    assert_eq!(version["@extra"], request_id);
    let version_string = version["value"].as_str().expect("a version string");
    assert!(!version_string.is_empty());

    // The download adapter's wire shapes, against the real parser
    // (TASK-260715-1onbmf). No authorization exists, so TDLib rejects each
    // request on its state — but it *parses* them first, and a mistyped
    // field name in `download.rs` is exactly what only the real library
    // rejects differently ("Failed to parse ..."). The mock cannot catch
    // that class; this probe does, with no api_id and no network.
    for payload in download_adapter_payloads() {
        let kind = payload["@type"].as_str().expect("payload names its type");
        let rejection = client
            .request(payload.clone())
            .expect("request submits")
            .wait_timeout(REAL_GUARD)
            .expect("TDLib answered the probe")
            .expect_err("no authorization exists, so the request is rejected");
        let TdError::Td { message, .. } = &rejection else {
            panic!("{kind}: expected a TDLib rejection, got {rejection:?}");
        };
        assert!(
            !message.contains("Failed to parse"),
            "{kind}: the real parser rejected the adapter's wire shape: {message}"
        );
    }

    // Clean client close: ok response, then authorizationStateClosed on
    // the update stream, then a closed stream.
    let closing = client.close().expect("close submits");
    assert!(
        closing
            .wait_timeout(REAL_GUARD)
            .expect("close answered")
            .is_ok()
    );
    loop {
        let update = updates
            .recv_timeout(REAL_GUARD)
            .expect("updates flow until the closed state");
        if update["@type"] == "updateAuthorizationState"
            && update["authorization_state"]["@type"] == "authorizationStateClosed"
        {
            break;
        }
    }
    assert_eq!(
        updates.recv_timeout(Duration::ZERO),
        Err(UpdateRecvError::Closed)
    );

    runtime.shutdown();
}
