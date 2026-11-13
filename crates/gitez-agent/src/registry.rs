//! The agents GitEZ knows, and whether each can run here.

use std::path::PathBuf;
use std::sync::Arc;

use crate::{Agent, AgentConfig, ClaudeCode};

/// Whether an agent's program was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    Installed { program: PathBuf },
    Unavailable { reason: String },
}

/// An agent and whether it can run, for Settings and the agent pickers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentStatus {
    pub id: &'static str,
    pub name: &'static str,
    pub availability: Availability,
}

/// A list of agents, looked up by id.
#[derive(Clone)]
pub struct Registry {
    agents: Vec<Arc<dyn Agent>>,
}

impl Registry {
    /// The agents GitEZ ships: Claude Code.
    pub fn builtin() -> Self {
        Self::with(vec![Arc::new(ClaudeCode)])
    }

    /// Exactly these agents (tests use a fake one).
    pub fn with(agents: Vec<Arc<dyn Agent>>) -> Self {
        Self { agents }
    }

    /// The agent with id `id`, if the registry has it.
    pub fn get(&self, id: &str) -> Option<Arc<dyn Agent>> {
        self.agents.iter().find(|agent| agent.id() == id).cloned()
    }

    pub fn all(&self) -> &[Arc<dyn Agent>] {
        &self.agents
    }

    /// Installed or unavailable, for each agent, with `config(id)`.
    pub fn statuses(&self, config: impl Fn(&str) -> AgentConfig) -> Vec<AgentStatus> {
        self.agents
            .iter()
            .map(|agent| AgentStatus {
                id: agent.id(),
                name: agent.name(),
                availability: match agent.locate(&config(agent.id())) {
                    Ok(program) => Availability::Installed { program },
                    Err(error) => Availability::Unavailable {
                        reason: error.to_string(),
                    },
                },
            })
            .collect()
    }
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.agents.iter().map(|agent| agent.id()))
            .finish()
    }
}
