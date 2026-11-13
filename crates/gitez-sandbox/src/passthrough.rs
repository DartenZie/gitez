//! A sandbox that is none, for tests.

use crate::{Policy, Result, Sandbox, Status, Wrapped};

/// The environment variable that asks a debug build to run threads in the
/// [`Passthrough`] sandbox, with the value `passthrough`.
pub const TEST_SANDBOX_ENV: &str = "GITEZ_TEST_SANDBOX";

/// Whether to run threads without a sandbox: only when `debug_build` (pass
/// `cfg!(debug_assertions)`) and `value`, the value of
/// [`TEST_SANDBOX_ENV`], is `passthrough`. A release build never does,
/// whatever the environment says.
pub fn passthrough_requested(debug_build: bool, value: Option<&std::ffi::OsStr>) -> bool {
    debug_build && value.is_some_and(|value| value == "passthrough")
}

/// [`passthrough_requested`] for this build and its environment.
pub fn passthrough_from_env(debug_build: bool) -> bool {
    passthrough_requested(debug_build, std::env::var_os(TEST_SANDBOX_ENV).as_deref())
}

/// Runs commands unchanged. For tests only: the supervisor uses it only in
/// debug builds when GITEZ_TEST_SANDBOX=passthrough (see D.9). Never offered
/// in the UI.
#[derive(Debug, Clone, Copy, Default)]
pub struct Passthrough;

impl Sandbox for Passthrough {
    fn name(&self) -> &'static str {
        "No sandbox (tests only)"
    }

    /// Always ready; there is no program.
    fn status(&self) -> Status {
        Status::Ready {
            program: std::path::PathBuf::new(),
            version: None,
        }
    }

    fn wrap(&self, command: gitez_agent::CommandSpec, _policy: &Policy) -> Result<Wrapped> {
        Ok(Wrapped::new(command, Box::new(())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Network;

    /// A shipped (release) build never runs a thread unsandboxed, whatever
    /// `GITEZ_TEST_SANDBOX` says; a debug build only with `passthrough`.
    #[test]
    fn only_a_debug_build_honours_the_test_sandbox() {
        use std::ffi::OsStr;
        for value in [
            None,
            Some(OsStr::new("")),
            Some(OsStr::new("passthrough")),
            Some(OsStr::new("PASSTHROUGH")),
            Some(OsStr::new("1")),
        ] {
            assert!(!passthrough_requested(false, value), "release, {value:?}");
        }
        assert!(passthrough_requested(true, Some(OsStr::new("passthrough"))));
        assert!(!passthrough_requested(true, None));
        assert!(!passthrough_requested(true, Some(OsStr::new("1"))));
        // As the supervisor and the GUI call it: this test build is a
        // debug one only when debug assertions are on.
        assert_eq!(
            passthrough_requested(cfg!(debug_assertions), Some(OsStr::new("passthrough"))),
            cfg!(debug_assertions)
        );
    }

    #[test]
    fn wraps_nothing() {
        let command = gitez_agent::CommandSpec {
            program: "/bin/echo".into(),
            args: vec!["hi".into()],
            env: vec![("A".into(), "1".into())],
            env_remove: vec!["B".into()],
            cwd: "/tmp".into(),
            stdin: Some("input".into()),
        };
        let policy = Policy {
            write: Vec::new(),
            deny_write: Vec::new(),
            deny_read: Vec::new(),
            network: Network::Hosts(Vec::new()),
            certificate_service: false,
        };
        let wrapped = Passthrough
            .wrap(command.clone(), &policy)
            .expect("it wraps");
        assert_eq!(wrapped.command, command);
        assert!(wrapped.blocked_hosts().is_empty());
        assert!(matches!(Passthrough.status(), Status::Ready { .. }));
    }
}
