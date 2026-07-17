//! TDLib configuration and storage policy for one account.
//!
//! This module turns an account's identity, storage layout, and platform
//! secrets into the ordered request sequence that initializes its TDLib
//! client — `setTdlibParameters`, then the memory/storage `setOption`s, then
//! an optional `addProxy` — and owns the policy choices those requests
//! encode (TASK-260715-1hdnuy).
//!
//! # The two-value shape
//!
//! Configuration is split so secrets have the shortest possible life:
//!
//! - [`AccountConfig`] is the *secret-free plan* — account id, per-account
//!   [`AccountStoragePaths`], device/app metadata, the storage and memory
//!   policies, the proxy. It derives `Debug` freely because it holds no
//!   never-logged material (the proxy's own credentials are [`Secret`], so
//!   even that field redacts).
//! - [`TdlibConfig`] is the plan with credentials attached, produced by
//!   [`AccountConfig::resolve`] against a [`SecretSource`]. It holds the
//!   `api_id`/`api_hash` and the database key, so its `Debug` is manual and
//!   redacts them; the plaintext reaches only the JSON request builders,
//!   which put it on the wire to TDLib and nowhere else.
//!
//! # Storage and memory policy (recorded evidence)
//!
//! GramDrive is a mirror: it must persist Telegram-derived state so a
//! restart resumes instead of re-crawling, and TDLib's local database is the
//! designed source of that (`.spec/architecture.md`, `LocalTdlibSource`).
//! [`StoragePolicy::mirror`] therefore enables the file, chat-info, and
//! message databases together (TDLib requires the lower two whenever the
//! message database is on) and disables secret chats, which GramDrive never
//! archives. Minimization then comes from [`MemoryOptions::minimal`]: it
//! keeps TDLib's own storage optimizer off — GramDrive owns the media cache
//! quota and its LRU (POL-2), and letting TDLib delete files it has not
//! promoted would corrupt that accounting — unloads messages from RAM
//! promptly, drops the persistent network-statistics database, and disables
//! notification groups the drive never surfaces. The TDLib per-client
//! memory concerns behind this (td#2516, td#2807) are the same ones that
//! excluded TDLib-in-extension in `.spec/architecture.md`.

mod secret;
mod storage;

pub use secret::{ApiCredentials, DatabaseKey, InMemorySecrets, Secret, SecretError, SecretSource};
pub use storage::{AccountStoragePaths, StorageLayout};

use std::fmt;

use gramdrive_model::identity::AccountId;
use serde_json::{Value, json};

/// Device and application metadata sent in `setTdlibParameters` and
/// disclosed to Telegram per its API terms (SEC-030).
///
/// The values must be truthful: the native adapter fills them from the real
/// device and the shipped product version. The [`Default`] is a safe
/// placeholder for tests and CI, not something to ship.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceMetadata {
    /// `device_model` — the hardware/app identity shown in Telegram's active
    /// sessions list.
    pub device_model: String,
    /// `system_version` — the OS version string.
    pub system_version: String,
    /// `application_version` — the shipped GramDrive version. The one field
    /// expected to change across an upgrade; the storage identity does not.
    pub application_version: String,
    /// `system_language_code` — a BCP-47 / ISO-639 language code.
    pub system_language_code: String,
}

impl Default for DeviceMetadata {
    fn default() -> DeviceMetadata {
        DeviceMetadata {
            device_model: "GramDrive".to_owned(),
            system_version: "unknown".to_owned(),
            application_version: "0.0.0".to_owned(),
            system_language_code: "en".to_owned(),
        }
    }
}

/// Which of TDLib's persistent databases an account uses
/// (`setTdlibParameters`).
///
/// The three database flags are not independent: TDLib rejects a
/// configuration whose `use_message_database` is set without both
/// `use_chat_info_database` and `use_file_database`. [`StoragePolicy::mirror`]
/// respects that; [`StoragePolicy::is_coherent`] states it explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoragePolicy {
    /// Persist file metadata and download state.
    pub use_file_database: bool,
    /// Persist chat/user info.
    pub use_chat_info_database: bool,
    /// Persist messages — the mirror's local source of history.
    pub use_message_database: bool,
    /// Enable secret chats. GramDrive never archives them; off by default.
    pub use_secret_chats: bool,
}

impl StoragePolicy {
    /// The GramDrive mirror default: file, chat-info, and message databases
    /// on; secret chats off.
    pub fn mirror() -> StoragePolicy {
        StoragePolicy {
            use_file_database: true,
            use_chat_info_database: true,
            use_message_database: true,
            use_secret_chats: false,
        }
    }

    /// Whether TDLib's database-dependency rule holds: enabling the message
    /// database requires the chat-info and file databases too.
    pub fn is_coherent(&self) -> bool {
        if self.use_message_database {
            self.use_chat_info_database && self.use_file_database
        } else if self.use_chat_info_database {
            self.use_file_database
        } else {
            true
        }
    }
}

impl Default for StoragePolicy {
    fn default() -> StoragePolicy {
        StoragePolicy::mirror()
    }
}

/// Memory- and storage-minimizing TDLib runtime options, applied as
/// `setOption` requests after `setTdlibParameters`.
///
/// The rationale for each choice is in the module docs; the values here are
/// the [`minimal`](MemoryOptions::minimal) defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryOptions {
    /// `use_storage_optimizer` — off: GramDrive owns the cache quota and its
    /// LRU (POL-2); TDLib must not delete unpromoted files.
    pub use_storage_optimizer: bool,
    /// `ignore_background_updates` — off: the mirror needs every update to
    /// stay correct, so updates are deliberately *not* ignored.
    pub ignore_background_updates: bool,
    /// `message_unload_delay` (seconds) — how long TDLib keeps a message in
    /// RAM after last access; low, to bound resident memory.
    pub message_unload_delay_secs: i64,
    /// `disable_persistent_network_statistics` — on: GramDrive does not use
    /// TDLib's network-stats database, so it is not written.
    pub disable_persistent_network_statistics: bool,
    /// `notification_group_count_max` — 0: GramDrive surfaces no Telegram
    /// notifications, so notification-group state is not tracked.
    pub notification_group_count_max: i64,
}

impl MemoryOptions {
    /// The minimal-footprint defaults described in the module docs.
    pub fn minimal() -> MemoryOptions {
        MemoryOptions {
            use_storage_optimizer: false,
            ignore_background_updates: false,
            message_unload_delay_secs: 60,
            disable_persistent_network_statistics: true,
            notification_group_count_max: 0,
        }
    }

    /// The ordered `setOption` requests for these options.
    fn option_requests(&self) -> Vec<Value> {
        vec![
            set_option_bool("use_storage_optimizer", self.use_storage_optimizer),
            set_option_bool("ignore_background_updates", self.ignore_background_updates),
            set_option_int("message_unload_delay", self.message_unload_delay_secs),
            set_option_bool(
                "disable_persistent_network_statistics",
                self.disable_persistent_network_statistics,
            ),
            set_option_int(
                "notification_group_count_max",
                self.notification_group_count_max,
            ),
        ]
    }
}

impl Default for MemoryOptions {
    fn default() -> MemoryOptions {
        MemoryOptions::minimal()
    }
}

/// Network path for an account's TDLib client (`addProxy`).
///
/// [`Proxy::Direct`] — the default — issues no request. The credential-
/// bearing variants keep their password/secret as [`Secret`], so the whole
/// enum redacts under `Debug`.
#[derive(Debug, Clone)]
pub enum Proxy {
    /// No proxy; TDLib connects directly.
    Direct,
    /// A SOCKS5 proxy.
    Socks5 {
        /// Proxy host.
        server: String,
        /// Proxy port.
        port: u16,
        /// Optional user name (not secret).
        username: Option<String>,
        /// Optional password.
        password: Option<Secret>,
    },
    /// An HTTP(S) `CONNECT` proxy.
    Http {
        /// Proxy host.
        server: String,
        /// Proxy port.
        port: u16,
        /// Optional user name (not secret).
        username: Option<String>,
        /// Optional password.
        password: Option<Secret>,
        /// Restrict to HTTP tunneling only.
        http_only: bool,
    },
    /// An MTProto proxy.
    Mtproto {
        /// Proxy host.
        server: String,
        /// Proxy port.
        port: u16,
        /// The proxy secret.
        secret: Secret,
    },
}

impl Proxy {
    /// The `addProxy` request, or `None` for [`Proxy::Direct`].
    fn add_request(&self) -> Option<Value> {
        let (server, port, proxy_type) = match self {
            Proxy::Direct => return None,
            Proxy::Socks5 {
                server,
                port,
                username,
                password,
            } => (
                server,
                port,
                json!({
                    "@type": "proxyTypeSocks5",
                    "username": username.clone().unwrap_or_default(),
                    "password": password.as_ref().map(Secret::expose).unwrap_or_default(),
                }),
            ),
            Proxy::Http {
                server,
                port,
                username,
                password,
                http_only,
            } => (
                server,
                port,
                json!({
                    "@type": "proxyTypeHttp",
                    "username": username.clone().unwrap_or_default(),
                    "password": password.as_ref().map(Secret::expose).unwrap_or_default(),
                    "http_only": http_only,
                }),
            ),
            Proxy::Mtproto {
                server,
                port,
                secret,
            } => (
                server,
                port,
                json!({ "@type": "proxyTypeMtproto", "secret": secret.expose() }),
            ),
        };
        Some(json!({
            "@type": "addProxy",
            "server": server,
            "port": port,
            "enable": true,
            "type": proxy_type,
        }))
    }
}

/// The secret-free configuration plan for one account.
///
/// Everything needed to configure an account's TDLib client except the
/// credentials; [`AccountConfig::resolve`] attaches those. Holds no
/// never-logged material, so it derives `Debug` and is the safe form to log.
#[derive(Debug, Clone)]
pub struct AccountConfig {
    /// The account this configuration is for.
    pub account: AccountId,
    /// Its isolated database and files directories.
    pub paths: AccountStoragePaths,
    /// Device/app metadata (SEC-030).
    pub device: DeviceMetadata,
    /// Which persistent databases the account uses.
    pub storage: StoragePolicy,
    /// Memory-minimizing runtime options.
    pub memory: MemoryOptions,
    /// Network path.
    pub proxy: Proxy,
    /// Whether to connect to Telegram's test data centers (test accounts).
    pub use_test_dc: bool,
}

impl AccountConfig {
    /// A mirror-default plan for `account` under `layout`: minimal memory
    /// options, no proxy, production data center. Callers override the
    /// fields they need (notably [`device`](AccountConfig::device) and, for
    /// test accounts, [`use_test_dc`](AccountConfig::use_test_dc)).
    pub fn mirror(account: AccountId, layout: &StorageLayout) -> AccountConfig {
        AccountConfig {
            account,
            paths: layout.account_paths(account),
            device: DeviceMetadata::default(),
            storage: StoragePolicy::mirror(),
            memory: MemoryOptions::minimal(),
            proxy: Proxy::Direct,
            use_test_dc: false,
        }
    }

    /// Attaches credentials from `secrets`, producing the wire-ready
    /// [`TdlibConfig`]. The product `api_id`/`api_hash` and this account's
    /// database key are read at runtime (SEC-003), never from the plan.
    pub fn resolve<S: SecretSource>(self, secrets: &S) -> Result<TdlibConfig, SecretError> {
        let credentials = secrets.api_credentials()?;
        let database_key = secrets.database_key(self.account)?;
        Ok(TdlibConfig {
            plan: self,
            credentials,
            database_key,
        })
    }
}

/// A resolved, wire-ready TDLib configuration: a plan plus its credentials.
///
/// Produced by [`AccountConfig::resolve`]. Holds the `api_id`/`api_hash` and
/// the database encryption key, so its `Debug` is manual and redacts them —
/// the plaintext leaves only through the request builders below, onto the
/// wire to TDLib.
#[derive(Clone)]
pub struct TdlibConfig {
    plan: AccountConfig,
    credentials: ApiCredentials,
    database_key: DatabaseKey,
}

impl TdlibConfig {
    /// The secret-free plan behind this configuration — the safe form to log.
    pub fn plan(&self) -> &AccountConfig {
        &self.plan
    }

    /// The `setTdlibParameters` request: storage paths, database flags,
    /// credentials, and device metadata. This is the one request that
    /// carries secret material (the api_hash and the base64 database key),
    /// and it goes only to TDLib.
    pub fn set_parameters_request(&self) -> Value {
        json!({
            "@type": "setTdlibParameters",
            "use_test_dc": self.plan.use_test_dc,
            "database_directory": self.plan.paths.database_directory_str(),
            "files_directory": self.plan.paths.files_directory_str(),
            "database_encryption_key": self.database_key.base64(),
            "use_file_database": self.plan.storage.use_file_database,
            "use_chat_info_database": self.plan.storage.use_chat_info_database,
            "use_message_database": self.plan.storage.use_message_database,
            "use_secret_chats": self.plan.storage.use_secret_chats,
            "api_id": self.credentials.api_id,
            "api_hash": self.credentials.api_hash.expose(),
            "system_language_code": self.plan.device.system_language_code,
            "device_model": self.plan.device.device_model,
            "system_version": self.plan.device.system_version,
            "application_version": self.plan.device.application_version,
        })
    }

    /// The memory/storage `setOption` requests, in order.
    pub fn option_requests(&self) -> Vec<Value> {
        self.plan.memory.option_requests()
    }

    /// The `addProxy` request, or `None` when the plan uses a direct
    /// connection.
    pub fn proxy_request(&self) -> Option<Value> {
        self.plan.proxy.add_request()
    }

    /// The full ordered initialization sequence: `setTdlibParameters` first
    /// (TDLib requires it before most requests), then the memory options,
    /// then `addProxy` if the plan uses a proxy. This is what the
    /// authorization flow submits to bring a client up.
    pub fn startup_requests(&self) -> Vec<Value> {
        let mut requests = Vec::with_capacity(1 + 5 + 1);
        requests.push(self.set_parameters_request());
        requests.extend(self.option_requests());
        if let Some(proxy) = self.proxy_request() {
            requests.push(proxy);
        }
        requests
    }
}

impl fmt::Debug for TdlibConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Credentials and the database key are never-logged material; the
        // plan is the loggable half.
        f.debug_struct("TdlibConfig")
            .field("plan", &self.plan)
            .field("credentials", &self.credentials)
            .field("database_key", &self.database_key)
            .finish()
    }
}

/// A `setOption` request carrying a boolean value.
fn set_option_bool(name: &str, value: bool) -> Value {
    json!({
        "@type": "setOption",
        "name": name,
        "value": { "@type": "optionValueBoolean", "value": value },
    })
}

/// A `setOption` request carrying an integer value.
fn set_option_int(name: &str, value: i64) -> Value {
    json!({
        "@type": "setOption",
        "name": name,
        "value": { "@type": "optionValueInteger", "value": value },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_secrets() -> InMemorySecrets {
        InMemorySecrets::new(ApiCredentials {
            api_id: 424242,
            api_hash: Secret::new("api-hash-sentinel"),
        })
        .with_key(
            AccountId(7),
            DatabaseKey::from_bytes(b"db-key-sentinel".to_vec()),
        )
    }

    fn resolved() -> TdlibConfig {
        let layout = StorageLayout::new("/root");
        AccountConfig::mirror(AccountId(7), &layout)
            .resolve(&test_secrets())
            .unwrap()
    }

    #[test]
    fn mirror_storage_policy_is_coherent() {
        assert!(StoragePolicy::mirror().is_coherent());
        // Message DB without its prerequisites is incoherent.
        assert!(
            !StoragePolicy {
                use_file_database: false,
                use_chat_info_database: false,
                use_message_database: true,
                use_secret_chats: false,
            }
            .is_coherent()
        );
    }

    #[test]
    fn set_parameters_request_carries_paths_flags_and_credentials() {
        let request = resolved().set_parameters_request();
        assert_eq!(request["@type"], "setTdlibParameters");
        assert_eq!(request["database_directory"], "/root/account-7/tdlib");
        assert_eq!(request["files_directory"], "/root/account-7/files");
        assert_eq!(request["use_message_database"], true);
        assert_eq!(request["use_secret_chats"], false);
        assert_eq!(request["api_id"], 424242);
        assert_eq!(request["api_hash"], "api-hash-sentinel");
        // base64("db-key-sentinel")
        assert_eq!(request["database_encryption_key"], "ZGIta2V5LXNlbnRpbmVs");
    }

    #[test]
    fn option_requests_are_the_minimal_set_in_order() {
        let requests = resolved().option_requests();
        let names: Vec<&str> = requests
            .iter()
            .map(|r| r["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "use_storage_optimizer",
                "ignore_background_updates",
                "message_unload_delay",
                "disable_persistent_network_statistics",
                "notification_group_count_max",
            ]
        );
        assert_eq!(requests[0]["value"]["value"], false);
        assert_eq!(requests[2]["value"]["value"], 60);
        assert_eq!(requests[4]["value"]["value"], 0);
    }

    #[test]
    fn direct_connection_emits_no_proxy_request() {
        assert!(resolved().proxy_request().is_none());
        // The startup sequence is params + five options, no proxy.
        assert_eq!(resolved().startup_requests().len(), 6);
    }

    #[test]
    fn socks5_proxy_emits_add_proxy_and_ends_the_startup_sequence() {
        let layout = StorageLayout::new("/root");
        let mut plan = AccountConfig::mirror(AccountId(7), &layout);
        plan.proxy = Proxy::Socks5 {
            server: "127.0.0.1".to_owned(),
            port: 1080,
            username: Some("user".to_owned()),
            password: Some(Secret::new("proxy-pw-sentinel")),
        };
        let config = plan.resolve(&test_secrets()).unwrap();

        let proxy = config.proxy_request().unwrap();
        assert_eq!(proxy["@type"], "addProxy");
        assert_eq!(proxy["server"], "127.0.0.1");
        assert_eq!(proxy["port"], 1080);
        assert_eq!(proxy["type"]["@type"], "proxyTypeSocks5");
        assert_eq!(proxy["type"]["password"], "proxy-pw-sentinel");

        let sequence = config.startup_requests();
        assert_eq!(sequence.len(), 7);
        assert_eq!(sequence[6]["@type"], "addProxy");
    }
}
