//! Live lanes: the active agent threads of the open repository, each a row
//! of the history above the commits with a dashed line down to the commit
//! its branch is at, as in the `C-2-Sessions` board. Only threads whose
//! agent holds a running slot (Preparing, Running) have a lane; queued,
//! waiting, finished and completed threads have none.
//!
//! The board's lanes are two lines high; the history is a list of rows of
//! one fixed height, so a lane is one line here: state, title, what the
//! agent does now, agent and network, time and steps.

use std::path::Path;

use gitez_agent_thread::{AgentThread, State, Timestamp};
use gitez_git::{Branch, Oid};
use gitez_sandbox::NetworkProfile;
use gpui_kit::component::StyledExt as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::agent_thread::{Look, agent_mark, chip, duration};
use crate::agent_threads::{AgentThreadKey, AgentThreads};
use crate::theme::color;

/// One active agent thread, as its lane shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lane {
    pub key: AgentThreadKey,
    pub title: SharedString,
    /// Preparing or Running.
    pub state: State,
    /// `agent/<id>`.
    pub branch: String,
    /// The commit the branch is at: its tip once it exists, else the commit
    /// the thread started from.
    pub tip: Option<Oid>,
    /// The tool the agent uses now, or "Setup" while preparing.
    pub tool: Option<SharedString>,
    /// What the tool works on.
    pub detail: SharedString,
    /// The agent's name, such as "Claude Code".
    pub agent: SharedString,
    /// The agent's id, for its mark.
    pub agent_id: SharedString,
    pub network: NetworkProfile,
    /// "just now", "14 min".
    pub elapsed: SharedString,
    pub steps: u32,
}

/// The lanes of the repository at `common_dir`: its threads that are
/// preparing or running, oldest first. `branches` are the repository's
/// local branches, for the tips of the threads' branches.
pub fn lanes(
    threads: &AgentThreads,
    common_dir: &Path,
    branches: &[Branch],
    now: Timestamp,
) -> Vec<Lane> {
    let Some(repository) = threads.repository(common_dir) else {
        return Vec::new();
    };
    let mut active: Vec<&AgentThread> = repository
        .threads
        .iter()
        .map(AsRef::as_ref)
        .filter(|thread| has_lane(thread.state))
        .collect();
    active.sort_by(|a, b| a.created.cmp(&b.created).then_with(|| a.id.cmp(&b.id)));
    active
        .into_iter()
        .map(|thread| {
            let agent = threads
                .agents()
                .get(&thread.agent)
                .map_or_else(|| thread.agent.clone(), |agent| agent.name().to_owned());
            lane(
                thread,
                repository.common_dir.clone(),
                agent.into(),
                branches,
                now,
            )
        })
        .collect()
}

/// Whether a thread in `state` has a lane.
pub fn has_lane(state: State) -> bool {
    matches!(state, State::Preparing | State::Running)
}

fn lane(
    thread: &AgentThread,
    common_dir: std::path::PathBuf,
    agent: SharedString,
    branches: &[Branch],
    now: Timestamp,
) -> Lane {
    let tip = branches
        .iter()
        .find(|branch| branch.name == thread.branch)
        .map(|branch| branch.target)
        .or_else(|| Oid::from_hex(thread.start.commit.as_bytes()).ok());
    let preparing = thread.state == State::Preparing;
    let (tool, detail) = match &thread.activity {
        Some(activity) => (activity.tool.clone(), activity.detail.clone()),
        None if preparing => (Some("Setup".to_owned()), "creating the worktree".to_owned()),
        None => (None, "starting the agent".to_owned()),
    };
    Lane {
        key: AgentThreadKey {
            common_dir,
            id: thread.id.clone(),
        },
        title: thread.title.clone().into(),
        state: thread.state,
        branch: thread.branch.clone(),
        tip,
        tool: tool.map(Into::into),
        detail: detail.into(),
        agent,
        agent_id: thread.agent.clone().into(),
        network: thread.network,
        elapsed: elapsed(thread.run_time(now).as_secs()).into(),
        steps: thread.steps(),
    }
}

/// "just now" for under a minute, then as the thread view says it.
fn elapsed(seconds: i64) -> String {
    if seconds < 60 {
        "just now".to_owned()
    } else {
        duration(seconds)
    }
}

impl Lane {
    /// "setting up" while preparing, else "1 step", "31 steps".
    pub fn steps_label(&self) -> String {
        match (self.state, self.steps) {
            (State::Preparing, _) => "setting up".to_owned(),
            (_, 1) => "1 step".to_owned(),
            (_, steps) => format!("{steps} steps"),
        }
    }

    /// The description column: state, title, then the tool and what it
    /// works on.
    pub fn description(&self, mono: SharedString) -> Div {
        let look = match self.state {
            State::Preparing => Look::Preparing,
            _ => Look::Running,
        };
        div()
            .h_flex()
            .flex_1()
            .min_w_0()
            .gap(px(7.))
            .child(chip(look, look.word().to_owned()))
            .child(
                div()
                    .flex_shrink(1.)
                    .min_w(px(60.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .font_weight(FontWeight::MEDIUM)
                    .child(self.title.clone()),
            )
            .when_some(self.tool.clone(), |this, tool| {
                this.child(
                    div()
                        .flex_none()
                        .h(px(17.))
                        .px(px(5.))
                        .h_flex()
                        .rounded(px(4.))
                        .border_1()
                        .border_color(rgb(color::TOOL_CHIP_BORDER))
                        .bg(rgb(color::TOOL_CHIP))
                        .font_family(mono.clone())
                        .text_size(px(10.5))
                        .text_color(rgb(color::TEXT_SECONDARY))
                        .child(tool),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_w(px(40.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .font_family(mono)
                    .text_size(px(11.))
                    .text_color(rgb(color::TEXT_MUTED))
                    .child(self.detail.clone()),
            )
    }

    /// The author and date columns, `width` wide together: the agent and
    /// its network, then the time and steps.
    pub fn agent_and_time(&self, width: Pixels) -> Div {
        div()
            .h_flex()
            .w(width)
            .flex_none()
            .gap(px(10.))
            .child(
                div()
                    .h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(7.))
                    .child(agent_mark(&self.agent_id))
                    .child(
                        div()
                            .flex_none()
                            .whitespace_nowrap()
                            .text_color(rgb(color::TEXT_SECONDARY))
                            .child(self.agent.clone()),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(px(11.5))
                            .text_color(rgb(color::TEXT_MUTED))
                            .child(self.network.label()),
                    ),
            )
            .child(
                div()
                    .h_flex()
                    .flex_none()
                    .gap(px(5.))
                    .whitespace_nowrap()
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(color::TEXT_SECONDARY))
                            .child(self.elapsed.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(color::TEXT_MUTED))
                            .child(self.steps_label()),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use gitez_agent_thread::State;

    use super::{elapsed, has_lane};

    #[test]
    fn only_preparing_and_running_threads_have_a_lane() {
        for (state, lane) in [
            (State::Queued, false),
            (State::Preparing, true),
            (State::Running, true),
            (State::Waiting, false),
            (State::Finished, false),
            (State::Completed, false),
        ] {
            assert_eq!(has_lane(state), lane, "{state:?}");
        }
    }

    #[test]
    fn elapsed_time_starts_as_just_now() {
        assert_eq!(elapsed(10), "just now");
        assert_eq!(elapsed(14 * 60), "14 min");
    }
}
