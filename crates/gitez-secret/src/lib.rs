//! Secret values that GitEZ gives to agent threads as environment variables.
//! The values are kept in the OS keychain (Keychain Services on macOS, the
//! Secret Service on Linux) and are never logged or written to GitEZ's own
//! files.
//!
//! Every call into [`Keychain`] talks to the OS keychain and may block (the
//! system can show a prompt or wait on D-Bus); call it from a background
//! task, never from the UI thread.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt;
use std::sync::{Mutex, PoisonError};

/// The environment variable that overrides the keychain service name.
pub const SERVICE_ENV: &str = "GITEZ_KEYCHAIN_SERVICE";

/// The keychain service name GitEZ uses when [`SERVICE_ENV`] is not set.
pub const DEFAULT_SERVICE: &str = "GitEZ";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No keychain on this system (Linux without a Secret Service).
    #[error("no keychain is available: {0}")]
    Unavailable(String),
    #[error("the keychain refused: {0}")]
    Keychain(String),
}
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Where secret values are kept. Values are never logged; `Debug` impls
/// must not print them.
pub trait SecretStore: Send + Sync {
    fn set(&self, account: &str, value: &str) -> Result<()>;
    /// `None` when nothing is stored for `account`.
    fn get(&self, account: &str) -> Result<Option<String>>;
    /// Deleting what is not there succeeds.
    fn delete(&self, account: &str) -> Result<()>;
}

/// The OS keychain, under one service name: the login keychain on macOS,
/// the default collection of the Secret Service on Linux.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keychain {
    service: String,
}

impl Keychain {
    /// The service GitEZ uses: `GITEZ_KEYCHAIN_SERVICE` if set and not
    /// empty, else "GitEZ".
    pub fn default_service() -> String {
        service_from(std::env::var_os(SERVICE_ENV))
    }

    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    /// The service name the values are kept under.
    pub fn service(&self) -> &str {
        &self.service
    }

    fn entry(&self, account: &str) -> Result<keyring::Entry> {
        keyring::Entry::new(&self.service, account).map_err(convert)
    }
}

impl SecretStore for Keychain {
    fn set(&self, account: &str, value: &str) -> Result<()> {
        self.entry(account)?.set_password(value).map_err(convert)
    }

    fn get(&self, account: &str) -> Result<Option<String>> {
        match self.entry(account)?.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(convert(err)),
        }
    }

    fn delete(&self, account: &str) -> Result<()> {
        match self.entry(account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(convert(err)),
        }
    }
}

/// The service name for the value of [`SERVICE_ENV`].
fn service_from(value: Option<OsString>) -> String {
    value
        .and_then(|value| value.into_string().ok())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_SERVICE.to_string())
}

/// A keyring error as ours. The messages of `keyring::Error` never contain
/// the secret itself (`BadEncoding` and `BadDataFormat` keep the bytes but
/// do not print them).
fn convert(err: keyring::Error) -> Error {
    match err {
        keyring::Error::NoDefaultStore => {
            let reason = match keyring::Entry::store_status() {
                Err(status) => status.to_string(),
                Ok(()) => err.to_string(),
            };
            Error::Unavailable(reason)
        }
        err => Error::Keychain(err.to_string()),
    }
}

/// Values in memory, for tests.
#[derive(Default)]
pub struct Memory {
    values: Mutex<HashMap<String, String>>,
}

impl Memory {
    fn values(&self) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
        // A panic while holding the lock cannot leave the map half-updated.
        self.values.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl fmt::Debug for Memory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let values = self.values();
        let mut accounts: Vec<&String> = values.keys().collect();
        accounts.sort();
        f.debug_struct("Memory")
            .field("accounts", &accounts)
            .finish_non_exhaustive()
    }
}

impl SecretStore for Memory {
    fn set(&self, account: &str, value: &str) -> Result<()> {
        self.values().insert(account.to_string(), value.to_string());
        Ok(())
    }

    fn get(&self, account: &str) -> Result<Option<String>> {
        Ok(self.values().get(account).cloned())
    }

    fn delete(&self, account: &str) -> Result<()> {
        self.values().remove(account);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Set, get, replace, get, delete, get, delete again, on any store.
    fn round_trip(store: &dyn SecretStore, account: &str) {
        assert_eq!(store.get(account).expect("get before set"), None);
        store.set(account, "first value").expect("set");
        assert_eq!(
            store.get(account).expect("get after set").as_deref(),
            Some("first value")
        );
        store.set(account, "sëcond välue ✓").expect("replace");
        assert_eq!(
            store.get(account).expect("get after replace").as_deref(),
            Some("sëcond välue ✓")
        );
        store.delete(account).expect("delete");
        assert_eq!(store.get(account).expect("get after delete"), None);
        store.delete(account).expect("delete again");
    }

    #[test]
    fn memory_round_trip() {
        let store = Memory::default();
        round_trip(&store, "all/GITHUB_TOKEN");
        round_trip(&store, "repo:/Users/me/repos/gitez/.git/SENTRY_DSN");
    }

    #[test]
    fn memory_keeps_accounts_apart() {
        let store = Memory::default();
        store.set("all/A", "one").expect("set A");
        store.set("all/B", "two").expect("set B");
        store.delete("all/A").expect("delete A");
        assert_eq!(store.get("all/A").expect("get A"), None);
        assert_eq!(store.get("all/B").expect("get B").as_deref(), Some("two"));
    }

    #[test]
    fn memory_debug_hides_values() {
        let store = Memory::default();
        store.set("all/TOKEN", "hunter2").expect("set");
        let debug = format!("{store:?}");
        assert!(debug.contains("all/TOKEN"), "{debug}");
        assert!(!debug.contains("hunter2"), "{debug}");
    }

    #[test]
    fn service_name() {
        assert_eq!(service_from(None), "GitEZ");
        assert_eq!(service_from(Some(OsString::new())), "GitEZ");
        assert_eq!(service_from(Some(OsString::from("GitEZ-dev"))), "GitEZ-dev");
    }

    /// A keychain under a service of its own (`GitEZ-test-<pid>-<n>`) that
    /// deletes the accounts it was given when dropped, also when a test
    /// fails.
    struct TestKeychain {
        keychain: Keychain,
        accounts: Vec<String>,
    }

    impl TestKeychain {
        /// `None` when this system has no keychain.
        fn new(accounts: &[&str]) -> Option<Self> {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let keychain = Keychain::new(format!("GitEZ-test-{}-{n}", std::process::id()));
            match keychain.get(accounts.first().copied().unwrap_or("probe")) {
                Err(Error::Unavailable(reason)) => {
                    println!("skipped: no keychain ({reason})");
                    return None;
                }
                Err(err) => panic!("the keychain failed: {err}"),
                Ok(_) => {}
            }
            Some(Self {
                keychain,
                accounts: accounts.iter().map(|a| a.to_string()).collect(),
            })
        }
    }

    impl Drop for TestKeychain {
        fn drop(&mut self) {
            for account in &self.accounts {
                if let Err(err) = self.keychain.delete(account) {
                    eprintln!(
                        "could not remove {account} from {}: {err}",
                        self.keychain.service
                    );
                }
            }
        }
    }

    #[test]
    fn keychain_round_trip() {
        const ACCOUNTS: &[&str] = &[
            "all/GITHUB_TOKEN",
            "repo:/Users/me/repos/gitez/.git/SENTRY_DSN",
        ];
        let Some(test) = TestKeychain::new(ACCOUNTS) else {
            return;
        };
        for account in ACCOUNTS {
            round_trip(&test.keychain, account);
        }
    }

    #[test]
    fn keychain_keeps_accounts_apart() {
        const ACCOUNTS: &[&str] = &["all/A", "all/B"];
        let Some(test) = TestKeychain::new(ACCOUNTS) else {
            return;
        };
        let store = &test.keychain;
        store.set("all/A", "one").expect("set A");
        store.set("all/B", "two").expect("set B");
        store.delete("all/A").expect("delete A");
        assert_eq!(store.get("all/A").expect("get A"), None);
        assert_eq!(store.get("all/B").expect("get B").as_deref(), Some("two"));
    }

    #[test]
    fn keychain_services_are_separate() {
        const ACCOUNTS: &[&str] = &["all/SHARED"];
        let (Some(first), Some(second)) =
            (TestKeychain::new(ACCOUNTS), TestKeychain::new(ACCOUNTS))
        else {
            return;
        };
        first
            .keychain
            .set("all/SHARED", "first")
            .expect("set first");
        assert_eq!(second.keychain.get("all/SHARED").expect("get second"), None);
        second
            .keychain
            .set("all/SHARED", "second")
            .expect("set second");
        assert_eq!(
            first
                .keychain
                .get("all/SHARED")
                .expect("get first")
                .as_deref(),
            Some("first")
        );
    }
}
