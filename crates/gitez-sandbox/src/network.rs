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

#[cfg(test)]
mod tests {
    use super::NetworkProfile;

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
