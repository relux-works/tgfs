//! Fixed bootstrap IPC boundary. The broker owns the Keychain credential and
//! exposes only the typed operations needed by the lander.

use std::io::{Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::eligibility::Attempt;
use crate::{AuditIntent, AuditOutcome, AuditRecord, BackendError, FixedBackend, SecretToken};

const MAX_FRAME: usize = 16 * 1024 * 1024;

#[derive(Debug, Serialize)]
#[serde(tag = "operation", rename_all = "kebab-case")]
enum Request<'a> {
    RecoverPending,
    ReadAttempt {
        pr: u64,
    },
    BeginAudit {
        record: &'a AuditRecord,
    },
    AcquireCredential,
    Advertise {
        handle: &'a [u8],
    },
    ReceivePack {
        handle: &'a [u8],
        request: &'a [u8],
    },
    ReadMain,
    FinishAudit {
        intent: &'a AuditIntent,
        outcome: &'static str,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "result", rename_all = "kebab-case")]
enum Response {
    Ok,
    Attempt { attempt: Box<Attempt> },
    Intent { intent: AuditIntent },
    Credential { handle: Vec<u8> },
    Bytes { bytes: Vec<u8> },
    Main { oid: String },
    Refused,
}

pub(crate) struct BootstrapBackend {
    socket: PathBuf,
}

impl BootstrapBackend {
    pub(crate) fn production() -> Self {
        Self {
            socket: production_socket(),
        }
    }

    fn exchange(&self, request: &Request<'_>) -> Result<Response, BackendError> {
        exchange(&self.socket, request)
    }
}

fn exchange(socket: &Path, request: &Request<'_>) -> Result<Response, BackendError> {
    validate_socket(socket)?;
    let mut stream = UnixStream::connect(socket).map_err(|_| BackendError)?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|_| BackendError)?;
    stream
        .set_write_timeout(Some(Duration::from_secs(30)))
        .map_err(|_| BackendError)?;
    let payload = serde_json::to_vec(request).map_err(|_| BackendError)?;
    let length = u32::try_from(payload.len()).map_err(|_| BackendError)?;
    stream
        .write_all(&length.to_be_bytes())
        .and_then(|()| stream.write_all(&payload))
        .and_then(|()| stream.flush())
        .map_err(|_| BackendError)?;

    let mut header = [0_u8; 4];
    stream.read_exact(&mut header).map_err(|_| BackendError)?;
    let response_length = usize::try_from(u32::from_be_bytes(header)).map_err(|_| BackendError)?;
    if response_length == 0 || response_length > MAX_FRAME {
        return Err(BackendError);
    }
    let mut response = vec![0_u8; response_length];
    stream.read_exact(&mut response).map_err(|_| BackendError)?;
    let mut trailing = [0_u8; 1];
    match stream.read(&mut trailing) {
        Ok(0) => {}
        Ok(_) | Err(_) => return Err(BackendError),
    }
    serde_json::from_slice(&response).map_err(|_| BackendError)
}

fn production_socket() -> PathBuf {
    // SAFETY: `geteuid` takes no arguments and has no memory-safety preconditions.
    let uid = unsafe { libc::geteuid() };
    PathBuf::from(format!(
        "/tmp/tgfs-ff-lander-bootstrap-{uid}/bootstrap.sock"
    ))
}

fn validate_socket(socket: &Path) -> Result<(), BackendError> {
    // SAFETY: `geteuid` takes no arguments and has no memory-safety preconditions.
    let uid = unsafe { libc::geteuid() };
    let parent = socket.parent().ok_or(BackendError)?;
    let parent_meta = std::fs::symlink_metadata(parent).map_err(|_| BackendError)?;
    let socket_meta = std::fs::symlink_metadata(socket).map_err(|_| BackendError)?;
    if !parent_meta.file_type().is_dir()
        || parent_meta.uid() != uid
        || parent_meta.permissions().mode() & 0o077 != 0
        || !socket_meta.file_type().is_socket()
        || socket_meta.uid() != uid
        || socket_meta.permissions().mode() & 0o177 != 0
    {
        return Err(BackendError);
    }
    Ok(())
}

impl FixedBackend for BootstrapBackend {
    fn now_unix(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|duration| i64::try_from(duration.as_secs()).ok())
            .unwrap_or(i64::MAX)
    }

    fn recover_pending_audits(&mut self) -> Result<(), BackendError> {
        match self.exchange(&Request::RecoverPending)? {
            Response::Ok => Ok(()),
            _ => Err(BackendError),
        }
    }

    fn read_attempt(&mut self, pr: u64) -> Result<Attempt, BackendError> {
        match self.exchange(&Request::ReadAttempt { pr })? {
            Response::Attempt { attempt } => Ok(*attempt),
            _ => Err(BackendError),
        }
    }

    fn begin_audit(&mut self, record: &AuditRecord) -> Result<AuditIntent, BackendError> {
        match self.exchange(&Request::BeginAudit { record })? {
            Response::Intent { intent } => Ok(intent),
            _ => Err(BackendError),
        }
    }

    fn mint_short_lived_token(&mut self) -> Result<SecretToken, BackendError> {
        match self.exchange(&Request::AcquireCredential)? {
            Response::Credential { handle } if !handle.is_empty() => Ok(SecretToken(handle)),
            _ => Err(BackendError),
        }
    }

    fn advertise_receive_pack(&mut self, token: &SecretToken) -> Result<Vec<u8>, BackendError> {
        match self.exchange(&Request::Advertise { handle: &token.0 })? {
            Response::Bytes { bytes } => Ok(bytes),
            _ => Err(BackendError),
        }
    }

    fn send_receive_pack(
        &mut self,
        token: &SecretToken,
        request: &[u8],
    ) -> Result<Vec<u8>, BackendError> {
        match self.exchange(&Request::ReceivePack {
            handle: &token.0,
            request,
        })? {
            Response::Bytes { bytes } => Ok(bytes),
            _ => Err(BackendError),
        }
    }

    fn read_main(&mut self) -> Result<String, BackendError> {
        match self.exchange(&Request::ReadMain)? {
            Response::Main { oid } => Ok(oid),
            _ => Err(BackendError),
        }
    }

    fn finish_audit(
        &mut self,
        intent: &AuditIntent,
        outcome: AuditOutcome,
    ) -> Result<(), BackendError> {
        let outcome = match outcome {
            AuditOutcome::Landed => "landed",
        };
        match self.exchange(&Request::FinishAudit { intent, outcome })? {
            Response::Ok => Ok(()),
            _ => Err(BackendError),
        }
    }
}
