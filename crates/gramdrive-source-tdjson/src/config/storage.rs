//! Per-account on-disk isolation and the logout wipe.
//!
//! Every account gets its own subtree under one root
//! ([`StorageLayout::account_dir`]); TDLib's database and downloaded files
//! both live inside it ([`AccountStoragePaths`]). Two properties fall out of
//! that shape and are what the fixtures assert:
//!
//! - **Isolation** — distinct [`AccountId`]s map to disjoint subtrees, so
//!   one account's TDLib database can never read or overwrite another's
//!   (SEC-041's local-first analogue).
//! - **Clean logout** — [`StorageLayout::wipe_account`] removes exactly one
//!   account's subtree and nothing else, the on-disk half of the SEC-004
//!   logout sequence. The keychain half — dropping the account's database
//!   key — is [`super::SecretStore::delete_account`], done by the native
//!   adapter over the OS keychain. The product `api_id`/`api_hash` are
//!   shared across accounts and app-lifetime, so they are not dropped on a
//!   per-account logout, only on a full app reset.

use std::path::{Path, PathBuf};

use gramdrive_model::identity::AccountId;

/// The root under which every account's TDLib state is stored.
///
/// The root itself is chosen by the native adapter — an app-container path
/// with least-privilege permissions (SEC-011) — and passed in; this type
/// only derives the per-account layout beneath it and never reaches outside
/// the root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageLayout {
    root: PathBuf,
}

impl StorageLayout {
    /// A layout rooted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> StorageLayout {
        StorageLayout { root: root.into() }
    }

    /// The root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The subtree that holds `account`'s entire TDLib state.
    ///
    /// The name is `account-<id>`, injective over [`AccountId`] (an `i64`),
    /// so distinct accounts never share a directory — the isolation
    /// guarantee. A negative id renders as `account--<n>`, which is a valid
    /// directory name and still injective.
    pub fn account_dir(&self, account: AccountId) -> PathBuf {
        self.root.join(format!("account-{}", account.0))
    }

    /// The TDLib database and files directories for `account`, both inside
    /// its [`account_dir`](StorageLayout::account_dir).
    pub fn account_paths(&self, account: AccountId) -> AccountStoragePaths {
        let base = self.account_dir(account);
        AccountStoragePaths {
            database_directory: base.join("tdlib"),
            files_directory: base.join("files"),
        }
    }

    /// Removes `account`'s entire subtree — the on-disk half of a clean
    /// logout (SEC-004).
    ///
    /// Only `account`'s own directory is touched, so sibling accounts are
    /// untouched by construction. A missing subtree is success, not an
    /// error: logout must converge whether or not TDLib ever wrote anything,
    /// so the operation is idempotent.
    pub fn wipe_account(&self, account: AccountId) -> std::io::Result<()> {
        let dir = self.account_dir(account);
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        }
    }
}

/// Where one account's TDLib database and downloaded files live.
///
/// Both directories sit inside the account's subtree, so the layout carries
/// the isolation and the wipe converges on removing their common parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountStoragePaths {
    database_directory: PathBuf,
    files_directory: PathBuf,
}

impl AccountStoragePaths {
    /// The TDLib database directory (`setTdlibParameters.database_directory`).
    pub fn database_directory(&self) -> &Path {
        &self.database_directory
    }

    /// The TDLib files directory (`setTdlibParameters.files_directory`).
    pub fn files_directory(&self) -> &Path {
        &self.files_directory
    }

    /// The database directory as the UTF-8 string TDLib's JSON interface
    /// takes. A non-UTF-8 root degrades lossily — acceptable because the
    /// native adapter roots the layout at a UTF-8 app-container path.
    pub(crate) fn database_directory_str(&self) -> String {
        self.database_directory.to_string_lossy().into_owned()
    }

    /// The files directory as a UTF-8 string; see
    /// [`database_directory_str`](AccountStoragePaths::database_directory_str).
    pub(crate) fn files_directory_str(&self) -> String {
        self.files_directory.to_string_lossy().into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinct_accounts_map_to_disjoint_subtrees() {
        let layout = StorageLayout::new("/root");
        let a = layout.account_paths(AccountId(1));
        let b = layout.account_paths(AccountId(2));

        assert_ne!(a.database_directory(), b.database_directory());
        assert_ne!(a.files_directory(), b.files_directory());
        // Neither account's subtree nests inside the other's.
        assert!(
            !a.database_directory()
                .starts_with(layout.account_dir(AccountId(2)))
        );
        assert!(
            !b.database_directory()
                .starts_with(layout.account_dir(AccountId(1)))
        );
    }

    #[test]
    fn account_paths_live_inside_the_account_dir() {
        let layout = StorageLayout::new("/root");
        let account = AccountId(-5);
        let dir = layout.account_dir(account);
        let paths = layout.account_paths(account);

        assert!(paths.database_directory().starts_with(&dir));
        assert!(paths.files_directory().starts_with(&dir));
        // A negative id is a valid, injective directory name.
        assert!(dir.ends_with("account--5"));
    }
}
