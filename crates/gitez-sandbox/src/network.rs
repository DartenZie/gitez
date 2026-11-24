//! How much network an agent thread's sandbox allows.

/// How much network a thread's sandbox allows.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum NetworkProfile {
    /// The agent's own API only.
    #[default]
    AgentOnly,
    /// Also package registries and the repository's remotes.
    Packages,
    /// Any host.
    Open,
}

impl NetworkProfile {
    pub const ALL: [NetworkProfile; 3] = [Self::AgentOnly, Self::Packages, Self::Open];

    /// "Agent API only", "Packages", "Open".
    pub fn label(self) -> &'static str {
        match self {
            Self::AgentOnly => "Agent API only",
            Self::Packages => "Packages",
            Self::Open => "Open",
        }
    }
}

/// Whether `host` can be listed as an allowed host: a domain name with at
/// least one dot (`example.com`), a wildcard under one (`*.example.com`),
/// `localhost`, an IPv4 address or a bracketed IPv6 address (`[::1]`), each
/// with an optional `:port`.
///
/// It is the check `srt` applies to `allowedDomains`, a little stricter
/// (only letters, digits, `-`, `_` and `.` in a name). `srt` refuses a whole
/// configuration over one entry it cannot read, such as an SSH host alias
/// from `~/.ssh/config` (`github-work`), so callers leave those out.
pub fn allowed_host(host: &str) -> bool {
    if let Some(rest) = host.strip_prefix('[') {
        let Some((address, after)) = rest.split_once(']') else {
            return false;
        };
        let address = address.split('%').next().unwrap_or(address);
        return address.parse::<std::net::Ipv6Addr>().is_ok()
            && (after.is_empty() || after.strip_prefix(':').is_some_and(port));
    }
    let name = match host.rsplit_once(':') {
        Some((name, suffix)) if port(suffix) => name,
        Some(_) => return false,
        None => host,
    };
    if name == "localhost" || name.parse::<std::net::Ipv4Addr>().is_ok() {
        return true;
    }
    // A wildcard needs a domain with a dot under it too (`*.com` is refused).
    let name = name.strip_prefix("*.").unwrap_or(name);
    let labels: Vec<&str> = name.split('.').collect();
    labels.len() >= 2
        && labels.iter().all(|label| {
            !label.is_empty()
                && label
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        })
}

/// A port number, 1 to 65535, without leading zeros.
fn port(text: &str) -> bool {
    !text.starts_with('0')
        && (1..=5).contains(&text.len())
        && text.chars().all(|c| c.is_ascii_digit())
        && text.parse::<u32>().is_ok_and(|port| port <= 65535)
}

#[cfg(test)]
mod tests {
    use super::{NetworkProfile, allowed_host};

    #[test]
    fn allowed_hosts_are_what_srt_accepts() {
        for host in [
            "github.com",
            "api.github.com",
            "*.crates.io",
            "example.com:8443",
            "localhost",
            "localhost:3000",
            "10.0.0.1",
            "10.0.0.1:22",
            "[::1]",
            "[2001:db8::1]:443",
            "my_host.example.org",
        ] {
            assert!(allowed_host(host), "{host} is allowed");
        }
        for host in [
            "",
            "github-work",
            "*",
            "*.com",
            "*.",
            "com.",
            ".com",
            "a..b",
            "::1",
            "[::1",
            "[::1]x",
            "[nope]",
            "example.com:0",
            "example.com:065",
            "example.com:65536",
            "example.com:x",
            "https://example.com",
            "example.com/path",
            "ex ample.com",
            "-oProxyCommand=x",
            "*.*.example.com",
        ] {
            assert!(!allowed_host(host), "{host:?} is refused");
        }
    }

    #[test]
    fn serialises_as_snake_case() {
        let names: Vec<String> = NetworkProfile::ALL
            .iter()
            .map(|profile| serde_json::to_string(profile).expect("a profile serialises"))
            .collect();
        assert_eq!(names, [r#""agent_only""#, r#""packages""#, r#""open""#]);
        for profile in NetworkProfile::ALL {
            let json = serde_json::to_string(&profile).expect("a profile serialises");
            let back: NetworkProfile = serde_json::from_str(&json).expect("a profile parses");
            assert_eq!(back, profile);
        }
    }

    #[test]
    fn default_is_agent_only() {
        assert_eq!(NetworkProfile::default(), NetworkProfile::AgentOnly);
    }

    #[test]
    fn labels() {
        let labels: Vec<&str> = NetworkProfile::ALL.map(NetworkProfile::label).to_vec();
        assert_eq!(labels, ["Agent API only", "Packages", "Open"]);
    }
}
