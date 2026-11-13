//! A sandbox that is none, for tests.

use crate::{Policy, Result, Sandbox, Status, Wrapped};

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
