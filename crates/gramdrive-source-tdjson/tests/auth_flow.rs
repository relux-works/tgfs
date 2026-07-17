//! Scripted authorization flows: the sans-IO [`AuthMachine`] driven over
//! the real runtime and the deterministic mock, with the mock's responder
//! playing Telegram's side (TASK-260715-51n6jb). Covers the acceptance
//! scenarios: success (phone/code/password and QR), retries, expired code,
//! invalid password, network loss mid-flow, cancellation, and unknown
//! TDLib states failing safe with typed errors.

// clippy.toml exempts test code; restated for the module-level test bodies
// of this integration binary (matching the established test-suite pattern).
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

mod common;

use common::{GUARD, ok_response, start_runtime, test_config};
use gramdrive_model::identity::AccountId;
use gramdrive_source_tdjson::{
    AccountConfig, ApiCredentials, AuthError, AuthInput, AuthMachine, AuthRejection, AuthState,
    DatabaseKey, InMemorySecrets, RetryAdvice, Secret, StorageLayout, TdClient, TdError,
    UpdateRecvError, UpdateStream,
};
use serde_json::{Value, json};

// --- Fixtures --------------------------------------------------------------

fn machine() -> AuthMachine {
    let secrets = InMemorySecrets::new(ApiCredentials {
        api_id: 424242,
        api_hash: Secret::new("api-hash-sentinel"),
    })
    .with_key(AccountId(7), DatabaseKey::from_bytes(b"key".to_vec()));
    let layout = StorageLayout::new("/root");
    let config = AccountConfig::mirror(AccountId(7), &layout)
        .resolve(&secrets)
        .unwrap();
    AuthMachine::new(config)
}

/// An `updateAuthorizationState` event for `client_id`.
fn auth_state_update(client_id: i32, state_json: &str) -> String {
    format!(
        concat!(
            r#"{{"@type":"updateAuthorizationState","#,
            r#""authorization_state":{},"@client_id":{}}}"#
        ),
        state_json, client_id
    )
}

/// An error response for request `extra` of `client_id`.
fn error_response(extra: u64, client_id: i32, code: i64, message: &str) -> String {
    format!(
        r#"{{"@type":"error","code":{code},"message":"{message}","@extra":{extra},"@client_id":{client_id}}}"#
    )
}

const WAIT_TDLIB_PARAMS: &str = r#"{"@type":"authorizationStateWaitTdlibParameters"}"#;
const WAIT_PHONE: &str = r#"{"@type":"authorizationStateWaitPhoneNumber"}"#;
const WAIT_PASSWORD: &str = concat!(
    r#"{"@type":"authorizationStateWaitPassword","#,
    r#""password_hint":"the usual","has_recovery_email_address":true}"#
);
const READY: &str = r#"{"@type":"authorizationStateReady"}"#;
const CLOSING: &str = r#"{"@type":"authorizationStateClosing"}"#;
const CLOSED: &str = r#"{"@type":"authorizationStateClosed"}"#;

fn wait_code(resend_timeout_secs: i64) -> String {
    format!(
        concat!(
            r#"{{"@type":"authorizationStateWaitCode","code_info":{{"#,
            r#""phone_number":"+15550100","#,
            r#""type":{{"@type":"authenticationCodeTypeTelegramMessage","length":5}},"#,
            r#""timeout":{}}}}}"#
        ),
        resend_timeout_secs
    )
}

// --- Driver ----------------------------------------------------------------
// The wiring the machine's module docs describe, in its test-sized form:
// pump updates into the machine, submit the requests each step returns,
// and submit inputs as the flow asks for them.

/// Submit `requests` in order and require each to resolve `ok`.
fn submit_all(client: &TdClient, requests: Vec<Value>) {
    for request in requests {
        let pending = client.request(request).unwrap();
        pending.wait_timeout(GUARD).expect("resolves").unwrap();
    }
}

/// Pump updates through `machine` until `stop` matches the current state.
fn advance_until(
    machine: &mut AuthMachine,
    client: &TdClient,
    updates: &UpdateStream,
    stop: impl Fn(&AuthState) -> bool,
) {
    while !stop(machine.state()) {
        let update = updates.recv_timeout(GUARD).unwrap();
        let step = machine.on_update(&update).unwrap();
        submit_all(client, step.requests);
    }
}

/// Turn `input` into its request, submit it, and return TDLib's answer.
fn submit_input(
    machine: &AuthMachine,
    client: &TdClient,
    input: AuthInput,
) -> Result<Value, TdError> {
    let request = machine.on_input(input).unwrap();
    client
        .request(request)
        .unwrap()
        .wait_timeout(GUARD)
        .expect("resolves")
}

// --- Scenarios -------------------------------------------------------------

#[test]
fn phone_code_password_success_path() {
    let (runtime, mock) = start_runtime(test_config());
    let (client, updates) = runtime.create_client().unwrap();
    let cid = client.client_id();
    mock.set_responder(move |sent| {
        let extra = sent.extra().unwrap();
        match sent.request_type().as_deref() {
            Some("setTdlibParameters") => {
                vec![ok_response(extra, cid), auth_state_update(cid, WAIT_PHONE)]
            }
            Some("setAuthenticationPhoneNumber") => {
                vec![
                    ok_response(extra, cid),
                    auth_state_update(cid, &wait_code(60)),
                ]
            }
            Some("checkAuthenticationCode") => {
                vec![
                    ok_response(extra, cid),
                    auth_state_update(cid, WAIT_PASSWORD),
                ]
            }
            Some("checkAuthenticationPassword") => {
                vec![ok_response(extra, cid), auth_state_update(cid, READY)]
            }
            _ => vec![ok_response(extra, cid)],
        }
    });

    let mut machine = machine();
    // The client wakes and TDLib reports the parameters state; the machine
    // answers with the startup sequence inside `advance_until`.
    mock.push_event(&auth_state_update(cid, WAIT_TDLIB_PARAMS));
    advance_until(&mut machine, &client, &updates, |state| {
        *state == AuthState::WaitPhoneNumber
    });

    submit_input(
        &machine,
        &client,
        AuthInput::SubmitPhoneNumber {
            phone_number: "+15550100".to_owned(),
        },
    )
    .unwrap();
    advance_until(&mut machine, &client, &updates, |state| {
        matches!(state, AuthState::WaitCode(_))
    });
    let AuthState::WaitCode(info) = machine.state() else {
        panic!("expected wait-code");
    };
    assert_eq!(info.phone_number, "+15550100");
    assert_eq!(info.code_length, Some(5));
    assert_eq!(info.resend_timeout_secs, Some(60));

    submit_input(
        &machine,
        &client,
        AuthInput::SubmitCode {
            code: Secret::new("13579"),
        },
    )
    .unwrap();
    advance_until(&mut machine, &client, &updates, |state| {
        matches!(state, AuthState::WaitPassword(_))
    });
    let AuthState::WaitPassword(info) = machine.state() else {
        panic!("expected wait-password");
    };
    assert_eq!(info.hint, "the usual");
    assert!(info.has_recovery_email);

    submit_input(
        &machine,
        &client,
        AuthInput::SubmitPassword {
            password: Secret::new("pw-sentinel"),
        },
    )
    .unwrap();
    advance_until(&mut machine, &client, &updates, |state| {
        *state == AuthState::Ready
    });

    // The wire saw exactly the expected conversation, in order.
    let types: Vec<String> = mock
        .take_sent()
        .iter()
        .filter_map(|sent| sent.request_type())
        .collect();
    assert_eq!(types[0], "setTdlibParameters");
    assert_eq!(types[1..6].iter().filter(|t| *t == "setOption").count(), 5);
    assert_eq!(
        types[6..],
        [
            "setAuthenticationPhoneNumber",
            "checkAuthenticationCode",
            "checkAuthenticationPassword",
        ]
    );
    // Nothing was absorbed silently along the way.
    assert_eq!(
        runtime.stats(),
        gramdrive_source_tdjson::RuntimeStats::default()
    );
}

#[test]
fn qr_confirmation_path_reaches_ready_through_password() {
    let (runtime, mock) = start_runtime(test_config());
    let (client, updates) = runtime.create_client().unwrap();
    let cid = client.client_id();
    mock.set_responder(move |sent| {
        let extra = sent.extra().unwrap();
        match sent.request_type().as_deref() {
            Some("setTdlibParameters") => {
                vec![ok_response(extra, cid), auth_state_update(cid, WAIT_PHONE)]
            }
            Some("requestQrCodeAuthentication") => vec![
                ok_response(extra, cid),
                auth_state_update(
                    cid,
                    concat!(
                        r#"{"@type":"authorizationStateWaitOtherDeviceConfirmation","#,
                        r#""link":"tg://login?token=first"}"#
                    ),
                ),
            ],
            Some("checkAuthenticationPassword") => {
                vec![ok_response(extra, cid), auth_state_update(cid, READY)]
            }
            _ => vec![ok_response(extra, cid)],
        }
    });

    let mut machine = machine();
    mock.push_event(&auth_state_update(cid, WAIT_TDLIB_PARAMS));
    advance_until(&mut machine, &client, &updates, |state| {
        *state == AuthState::WaitPhoneNumber
    });

    submit_input(&machine, &client, AuthInput::RequestQrCode).unwrap();
    advance_until(&mut machine, &client, &updates, |state| {
        matches!(state, AuthState::WaitQrConfirmation { .. })
    });
    assert_eq!(
        machine.state(),
        &AuthState::WaitQrConfirmation {
            link: "tg://login?token=first".to_owned(),
        }
    );

    // TDLib rotates the token: the same state arrives again with a fresh
    // link, and the machine reports it as entered.
    mock.push_event(&auth_state_update(
        cid,
        concat!(
            r#"{"@type":"authorizationStateWaitOtherDeviceConfirmation","#,
            r#""link":"tg://login?token=second"}"#
        ),
    ));
    let update = updates.recv_timeout(GUARD).unwrap();
    let step = machine.on_update(&update).unwrap();
    assert_eq!(
        step.entered,
        Some(AuthState::WaitQrConfirmation {
            link: "tg://login?token=second".to_owned(),
        })
    );

    // Another device scans and confirms; the account has 2FA, so TDLib
    // moves to the password gate.
    mock.push_event(&auth_state_update(cid, WAIT_PASSWORD));
    advance_until(&mut machine, &client, &updates, |state| {
        matches!(state, AuthState::WaitPassword(_))
    });
    submit_input(
        &machine,
        &client,
        AuthInput::SubmitPassword {
            password: Secret::new("pw-sentinel"),
        },
    )
    .unwrap();
    advance_until(&mut machine, &client, &updates, |state| {
        *state == AuthState::Ready
    });
}

#[test]
fn wrong_code_classifies_and_the_retry_succeeds() {
    let (runtime, mock) = start_runtime(test_config());
    let (client, updates) = runtime.create_client().unwrap();
    let cid = client.client_id();
    let mut code_attempts = 0;
    mock.set_responder(move |sent| {
        let extra = sent.extra().unwrap();
        match sent.request_type().as_deref() {
            Some("setTdlibParameters") => {
                vec![ok_response(extra, cid), auth_state_update(cid, WAIT_PHONE)]
            }
            Some("setAuthenticationPhoneNumber") => {
                vec![
                    ok_response(extra, cid),
                    auth_state_update(cid, &wait_code(60)),
                ]
            }
            Some("checkAuthenticationCode") => {
                code_attempts += 1;
                if code_attempts == 1 {
                    // Rejected; TDLib stays in wait-code and sends no update.
                    vec![error_response(extra, cid, 400, "PHONE_CODE_INVALID")]
                } else {
                    // This account has no 2FA: straight to ready.
                    vec![ok_response(extra, cid), auth_state_update(cid, READY)]
                }
            }
            _ => vec![ok_response(extra, cid)],
        }
    });

    let mut machine = machine();
    mock.push_event(&auth_state_update(cid, WAIT_TDLIB_PARAMS));
    advance_until(&mut machine, &client, &updates, |state| {
        *state == AuthState::WaitPhoneNumber
    });
    submit_input(
        &machine,
        &client,
        AuthInput::SubmitPhoneNumber {
            phone_number: "+15550100".to_owned(),
        },
    )
    .unwrap();
    advance_until(&mut machine, &client, &updates, |state| {
        matches!(state, AuthState::WaitCode(_))
    });

    let error = submit_input(
        &machine,
        &client,
        AuthInput::SubmitCode {
            code: Secret::new("00000"),
        },
    )
    .unwrap_err();
    let rejection = AuthRejection::classify(&error);
    assert_eq!(rejection, AuthRejection::InvalidCode);
    assert_eq!(rejection.advice(), RetryAdvice::ReviseInput);
    // The flow position is exactly where TDLib says it is.
    assert!(matches!(machine.state(), AuthState::WaitCode(_)));

    submit_input(
        &machine,
        &client,
        AuthInput::SubmitCode {
            code: Secret::new("13579"),
        },
    )
    .unwrap();
    advance_until(&mut machine, &client, &updates, |state| {
        *state == AuthState::Ready
    });
}

#[test]
fn expired_code_recovers_through_resend() {
    let (runtime, mock) = start_runtime(test_config());
    let (client, updates) = runtime.create_client().unwrap();
    let cid = client.client_id();
    let mut resent = false;
    mock.set_responder(move |sent| {
        let extra = sent.extra().unwrap();
        match sent.request_type().as_deref() {
            Some("setTdlibParameters") => {
                vec![ok_response(extra, cid), auth_state_update(cid, WAIT_PHONE)]
            }
            Some("setAuthenticationPhoneNumber") => {
                vec![
                    ok_response(extra, cid),
                    auth_state_update(cid, &wait_code(60)),
                ]
            }
            Some("resendAuthenticationCode") => {
                resent = true;
                // A fresh code: the same state re-enters with new info.
                vec![
                    ok_response(extra, cid),
                    auth_state_update(cid, &wait_code(90)),
                ]
            }
            Some("checkAuthenticationCode") => {
                if resent {
                    vec![ok_response(extra, cid), auth_state_update(cid, READY)]
                } else {
                    vec![error_response(extra, cid, 400, "PHONE_CODE_EXPIRED")]
                }
            }
            _ => vec![ok_response(extra, cid)],
        }
    });

    let mut machine = machine();
    mock.push_event(&auth_state_update(cid, WAIT_TDLIB_PARAMS));
    advance_until(&mut machine, &client, &updates, |state| {
        *state == AuthState::WaitPhoneNumber
    });
    submit_input(
        &machine,
        &client,
        AuthInput::SubmitPhoneNumber {
            phone_number: "+15550100".to_owned(),
        },
    )
    .unwrap();
    advance_until(&mut machine, &client, &updates, |state| {
        matches!(state, AuthState::WaitCode(_))
    });

    let error = submit_input(
        &machine,
        &client,
        AuthInput::SubmitCode {
            code: Secret::new("13579"),
        },
    )
    .unwrap_err();
    let rejection = AuthRejection::classify(&error);
    assert_eq!(rejection, AuthRejection::ExpiredCode);
    assert_eq!(rejection.advice(), RetryAdvice::RequestNewCode);

    // Follow the advice: resend, receive the fresh code info, retry.
    submit_input(&machine, &client, AuthInput::ResendCode).unwrap();
    advance_until(
        &mut machine,
        &client,
        &updates,
        |state| matches!(state, AuthState::WaitCode(info) if info.resend_timeout_secs == Some(90)),
    );
    submit_input(
        &machine,
        &client,
        AuthInput::SubmitCode {
            code: Secret::new("24680"),
        },
    )
    .unwrap();
    advance_until(&mut machine, &client, &updates, |state| {
        *state == AuthState::Ready
    });
}

#[test]
fn invalid_password_classifies_and_the_retry_succeeds() {
    let (runtime, mock) = start_runtime(test_config());
    let (client, updates) = runtime.create_client().unwrap();
    let cid = client.client_id();
    let mut password_attempts = 0;
    mock.set_responder(move |sent| {
        let extra = sent.extra().unwrap();
        match sent.request_type().as_deref() {
            Some("setTdlibParameters") => {
                vec![
                    ok_response(extra, cid),
                    auth_state_update(cid, WAIT_PASSWORD),
                ]
            }
            Some("checkAuthenticationPassword") => {
                password_attempts += 1;
                if password_attempts == 1 {
                    vec![error_response(extra, cid, 400, "PASSWORD_HASH_INVALID")]
                } else {
                    vec![ok_response(extra, cid), auth_state_update(cid, READY)]
                }
            }
            _ => vec![ok_response(extra, cid)],
        }
    });

    let mut machine = machine();
    // Resume shape: TDLib reports the password gate straight away (the
    // phone/code legs completed in an earlier run).
    mock.push_event(&auth_state_update(cid, WAIT_TDLIB_PARAMS));
    advance_until(&mut machine, &client, &updates, |state| {
        matches!(state, AuthState::WaitPassword(_))
    });

    let error = submit_input(
        &machine,
        &client,
        AuthInput::SubmitPassword {
            password: Secret::new("wrong"),
        },
    )
    .unwrap_err();
    let rejection = AuthRejection::classify(&error);
    assert_eq!(rejection, AuthRejection::InvalidPassword);
    assert_eq!(rejection.advice(), RetryAdvice::ReviseInput);
    assert!(matches!(machine.state(), AuthState::WaitPassword(_)));

    submit_input(
        &machine,
        &client,
        AuthInput::SubmitPassword {
            password: Secret::new("right"),
        },
    )
    .unwrap();
    advance_until(&mut machine, &client, &updates, |state| {
        *state == AuthState::Ready
    });
}

#[test]
fn network_loss_mid_flow_is_transient_and_the_same_input_retries() {
    let (runtime, mock) = start_runtime(test_config());
    let (client, updates) = runtime.create_client().unwrap();
    let cid = client.client_id();
    let mut code_attempts = 0;
    mock.set_responder(move |sent| {
        let extra = sent.extra().unwrap();
        match sent.request_type().as_deref() {
            Some("setTdlibParameters") => {
                vec![
                    ok_response(extra, cid),
                    auth_state_update(cid, &wait_code(60)),
                ]
            }
            Some("checkAuthenticationCode") => {
                code_attempts += 1;
                if code_attempts == 1 {
                    vec![error_response(
                        extra,
                        cid,
                        500,
                        "Failed to connect to Telegram servers",
                    )]
                } else {
                    vec![ok_response(extra, cid), auth_state_update(cid, READY)]
                }
            }
            _ => vec![ok_response(extra, cid)],
        }
    });

    let mut machine = machine();
    mock.push_event(&auth_state_update(cid, WAIT_TDLIB_PARAMS));
    advance_until(&mut machine, &client, &updates, |state| {
        matches!(state, AuthState::WaitCode(_))
    });

    // The network drops: TDLib reports it through the connection-state
    // update, which is not an authorization transition — the machine
    // ignores it and holds its position.
    mock.push_event(&format!(
        concat!(
            r#"{{"@type":"updateConnectionState","#,
            r#""state":{{"@type":"connectionStateWaitingForNetwork"}},"@client_id":{}}}"#
        ),
        cid
    ));
    let update = updates.recv_timeout(GUARD).unwrap();
    let step = machine.on_update(&update).unwrap();
    assert!(step.entered.is_none());
    assert!(matches!(machine.state(), AuthState::WaitCode(_)));

    let error = submit_input(
        &machine,
        &client,
        AuthInput::SubmitCode {
            code: Secret::new("13579"),
        },
    )
    .unwrap_err();
    let rejection = AuthRejection::classify(&error);
    assert_eq!(rejection, AuthRejection::Network);
    assert_eq!(rejection.advice(), RetryAdvice::RetrySameInput);
    assert!(matches!(machine.state(), AuthState::WaitCode(_)));

    // The network returns; the very same input goes through.
    submit_input(
        &machine,
        &client,
        AuthInput::SubmitCode {
            code: Secret::new("13579"),
        },
    )
    .unwrap();
    advance_until(&mut machine, &client, &updates, |state| {
        *state == AuthState::Ready
    });
}

#[test]
fn cancellation_mid_flow_closes_the_client_and_further_input_is_typed() {
    let (runtime, mock) = start_runtime(test_config());
    let (client, updates) = runtime.create_client().unwrap();
    let cid = client.client_id();
    mock.set_responder(move |sent| {
        let extra = sent.extra().unwrap();
        match sent.request_type().as_deref() {
            Some("setTdlibParameters") => {
                vec![
                    ok_response(extra, cid),
                    auth_state_update(cid, &wait_code(60)),
                ]
            }
            // The code submission never gets an answer — the user gives up
            // on it, then cancels the whole flow.
            Some("checkAuthenticationCode") => Vec::new(),
            Some("close") => vec![
                ok_response(extra, cid),
                auth_state_update(cid, CLOSING),
                auth_state_update(cid, CLOSED),
            ],
            _ => vec![ok_response(extra, cid)],
        }
    });

    let mut machine = machine();
    mock.push_event(&auth_state_update(cid, WAIT_TDLIB_PARAMS));
    advance_until(&mut machine, &client, &updates, |state| {
        matches!(state, AuthState::WaitCode(_))
    });

    // A submission is in flight; the user abandons it (drop = cancel: the
    // runtime discards any late answer rather than delivering it).
    let request = machine
        .on_input(AuthInput::SubmitCode {
            code: Secret::new("13579"),
        })
        .unwrap();
    let in_flight = client.request(request).unwrap();
    in_flight.cancel();

    // Cancel the flow: close, then follow TDLib down to closed.
    submit_input(&machine, &client, AuthInput::Cancel).unwrap();
    advance_until(&mut machine, &client, &updates, |state| {
        *state == AuthState::Closed
    });

    // The machine refuses further input with a typed error…
    assert!(matches!(
        machine.on_input(AuthInput::SubmitCode {
            code: Secret::new("13579"),
        }),
        Err(AuthError::InvalidInput {
            state: "closed",
            ..
        })
    ));
    // …and the runtime has ended the client underneath it.
    assert_eq!(
        client.request(json!({"@type": "ping"})).map(drop),
        Err(TdError::ClientClosed)
    );
    assert_eq!(
        updates.recv_timeout(std::time::Duration::ZERO),
        Err(UpdateRecvError::Closed)
    );
}

#[test]
fn unknown_states_fail_safe_and_cancel_still_escapes() {
    let (runtime, mock) = start_runtime(test_config());
    let (client, updates) = runtime.create_client().unwrap();
    let cid = client.client_id();
    mock.set_responder(move |sent| {
        let extra = sent.extra().unwrap();
        match sent.request_type().as_deref() {
            Some("setTdlibParameters") => {
                vec![ok_response(extra, cid), auth_state_update(cid, WAIT_PHONE)]
            }
            Some("close") => vec![
                ok_response(extra, cid),
                auth_state_update(cid, CLOSING),
                auth_state_update(cid, CLOSED),
            ],
            _ => vec![ok_response(extra, cid)],
        }
    });

    let mut machine = machine();
    mock.push_event(&auth_state_update(cid, WAIT_TDLIB_PARAMS));
    advance_until(&mut machine, &client, &updates, |state| {
        *state == AuthState::WaitPhoneNumber
    });

    // TDLib (a newer one, or an email-gated account) reports a state this
    // machine does not support: a typed state, not a panic, not a wedge.
    mock.push_event(&auth_state_update(
        cid,
        r#"{"@type":"authorizationStateWaitEmailAddress","allow_apple_id":false}"#,
    ));
    let update = updates.recv_timeout(GUARD).unwrap();
    let step = machine.on_update(&update).unwrap();
    assert_eq!(
        step.entered,
        Some(AuthState::Unsupported {
            td_type: "authorizationStateWaitEmailAddress".to_owned(),
        })
    );

    // Every flow input fails with the typed unsupported error…
    assert_eq!(
        machine.on_input(AuthInput::SubmitPhoneNumber {
            phone_number: "+15550100".to_owned(),
        }),
        Err(AuthError::UnsupportedState {
            td_type: "authorizationStateWaitEmailAddress".to_owned(),
        })
    );
    // …while cancel still closes the client cleanly.
    submit_input(&machine, &client, AuthInput::Cancel).unwrap();
    advance_until(&mut machine, &client, &updates, |state| {
        *state == AuthState::Closed
    });
    let _ = runtime;
}
