//! What an agent thread's agent did, as its activity list shows it: the
//! events of each turn's transcript turned into entries, and the entries
//! into rows, with long runs of tool steps collapsed.

use std::collections::HashSet;

use gitez_agent::AgentEvent;
use gitez_agent_thread::TurnKind;

/// Tool steps in a row shown before the rest of the row collapses.
pub const SHOWN_IN_A_RUN: usize = 2;
/// Tool names listed on a collapsed run, before "…".
const NAMES_SHOWN: usize = 3;

/// One turn of an agent thread: why it ran, and what its agent reported.
#[derive(Debug, Clone, Copy)]
pub struct TurnEvents<'a> {
    pub kind: TurnKind,
    pub prompt: &'a str,
    pub events: &'a [AgentEvent],
}

/// Something in the activity list.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    /// The prompt of later turn `turn`: a reply, or review comments.
    Prompt {
        kind: TurnKind,
        text: String,
        turn: usize,
    },
    /// GitEZ continued the conversation: a retry, or after the usage limit.
    Continued(TurnKind),
    /// The agent wrote this.
    Text(String),
    /// The agent used a tool on `detail`; `failure` is the first line of why
    /// it failed, if it did.
    Tool {
        name: String,
        detail: String,
        failure: Option<String>,
    },
}

/// A row of the activity list.
#[derive(Debug, Clone, PartialEq)]
pub enum Row<'a> {
    Prompt {
        kind: TurnKind,
        text: &'a str,
        /// The turn's index, for its images.
        turn: usize,
    },
    Continued(TurnKind),
    Text(&'a str),
    Tool {
        name: &'a str,
        detail: &'a str,
        failure: Option<&'a str>,
        /// The step the agent is on now.
        live: bool,
    },
    /// Steps left out: "Show `count` more steps · `names`". `group` is the
    /// index of the first one, to expand them with.
    More {
        group: usize,
        count: usize,
        names: String,
    },
}

/// The entries of `turns`, in order. The first turn's prompt is shown
/// above the activity, so only later turns' prompts are entries. Each
/// turn's last text is left out when it is the turn's final message, which
/// the thread view shows in full below.
pub fn entries(turns: &[TurnEvents<'_>]) -> Vec<Entry> {
    let mut entries = Vec::new();
    for (index, turn) in turns.iter().enumerate() {
        if index > 0 {
            entries.push(match turn.kind {
                TurnKind::Reply | TurnKind::Review | TurnKind::Start => Entry::Prompt {
                    kind: turn.kind,
                    text: turn.prompt.to_owned(),
                    turn: index,
                },
                // GitEZ's own words, not the user's: named, not shown.
                TurnKind::Retry | TurnKind::Resume | TurnKind::Brief => Entry::Continued(turn.kind),
            });
        }
        let first = entries.len();
        let mut final_text = None;
        for event in turn.events {
            match event {
                AgentEvent::Text(text) => entries.push(Entry::Text(text.clone())),
                AgentEvent::Tool { name, detail } => entries.push(Entry::Tool {
                    name: name.clone(),
                    detail: detail.clone(),
                    failure: None,
                }),
                AgentEvent::ToolFailed(why) => {
                    // A tool's result follows its use; it belongs to the
                    // last tool of this turn.
                    let last_tool =
                        entries[first..]
                            .iter_mut()
                            .rev()
                            .find_map(|entry| match entry {
                                Entry::Tool { failure, .. } => Some(failure),
                                _ => None,
                            });
                    if let Some(failure) = last_tool {
                        failure.get_or_insert_with(|| why.clone());
                    }
                }
                AgentEvent::Finished(finished) => final_text = Some(finished.text.trim()),
                AgentEvent::Started { .. } | AgentEvent::UsageLimit(_) => {}
            }
        }
        if let Some(final_text) = final_text.filter(|text| !text.is_empty()) {
            let last_text = entries[first..]
                .iter()
                .rposition(|entry| matches!(entry, Entry::Text(_)));
            if let Some(position) = last_text
                && matches!(&entries[first + position], Entry::Text(text) if text.trim() == final_text)
            {
                entries.remove(first + position);
            }
        }
    }
    entries
}

/// A row of the activity list by where it comes from, so a long list can
/// be kept and each row read from the entries only when it is drawn.
#[derive(Debug, Clone, PartialEq)]
pub enum Planned {
    /// Entry `index`; `live` when it is the step the agent is on now.
    Entry { index: usize, live: bool },
    /// Steps left out, as [`Row::More`].
    More {
        group: usize,
        count: usize,
        names: String,
    },
}

/// The rows showing `entries`. In each run of tool steps that did not fail,
/// the steps after the first [`SHOWN_IN_A_RUN`] collapse into one
/// [`Row::More`], unless its `group` is in `expanded` or fewer than two
/// would collapse. While `live`, the last entry is the step running now: it
/// is never collapsed.
#[cfg(test)]
pub fn rows<'a>(entries: &'a [Entry], expanded: &HashSet<usize>, live: bool) -> Vec<Row<'a>> {
    plan(entries, expanded, live)
        .iter()
        .map(|planned| row_of(entries, planned))
        .collect()
}

/// [`rows`] as [`Planned`] rows.
pub fn plan(entries: &[Entry], expanded: &HashSet<usize>, live: bool) -> Vec<Planned> {
    let live_index = live.then(|| entries.len().checked_sub(1)).flatten();
    let entry = |index: usize| Planned::Entry {
        index,
        live: Some(index) == live_index,
    };
    let mut rows = Vec::with_capacity(entries.len());
    let mut index = 0;
    while index < entries.len() {
        let in_run = |index: usize| {
            matches!(&entries[index], Entry::Tool { failure: None, .. })
                && Some(index) != live_index
        };
        if !in_run(index) {
            rows.push(entry(index));
            index += 1;
            continue;
        }
        let start = index;
        let mut end = index;
        while end < entries.len() && in_run(end) {
            end += 1;
        }
        let hidden = start + SHOWN_IN_A_RUN..end;
        if hidden.len() < 2 || expanded.contains(&hidden.start) {
            rows.extend((start..end).map(entry));
        } else {
            rows.extend((start..hidden.start).map(entry));
            rows.push(Planned::More {
                group: hidden.start,
                count: hidden.len(),
                names: tool_names(&entries[hidden]),
            });
        }
        index = end;
    }
    rows
}

/// The row `planned` names, read from `entries` (an empty text if it is
/// out of range).
pub fn row_of<'a>(entries: &'a [Entry], planned: &Planned) -> Row<'a> {
    match planned {
        Planned::Entry { index, live } => match entries.get(*index) {
            Some(Entry::Prompt { kind, text, turn }) => Row::Prompt {
                kind: *kind,
                text,
                turn: *turn,
            },
            Some(Entry::Continued(kind)) => Row::Continued(*kind),
            Some(Entry::Text(text)) => Row::Text(text),
            Some(Entry::Tool {
                name,
                detail,
                failure,
            }) => Row::Tool {
                name,
                detail,
                failure: failure.as_deref(),
                live: *live,
            },
            None => Row::Text(""),
        },
        Planned::More {
            group,
            count,
            names,
        } => Row::More {
            group: *group,
            count: *count,
            names: names.clone(),
        },
    }
}

/// What turns the list `old` into `new`: the range of `old` that changed
/// and how many rows of `new` replace it, keeping the rows before and
/// after (and so their measured heights and the scroll position).
pub fn changed(old: &[Planned], new: &[Planned]) -> (std::ops::Range<usize>, usize) {
    let prefix = old
        .iter()
        .zip(new)
        .take_while(|(old, new)| old == new)
        .count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(old, new)| old == new)
        .count();
    (prefix..old.len() - suffix, new.len() - prefix - suffix)
}

/// "Read, Grep": the tools of `entries`, each once, in the order they were
/// first used.
fn tool_names(entries: &[Entry]) -> String {
    let mut names: Vec<&str> = Vec::new();
    for entry in entries {
        if let Entry::Tool { name, .. } = entry
            && !names.contains(&name.as_str())
        {
            names.push(name);
        }
    }
    let more = names.len() > NAMES_SHOWN;
    names.truncate(NAMES_SHOWN);
    let mut text = names.join(", ");
    if more {
        text.push_str(", …");
    }
    text
}

#[cfg(test)]
mod tests {
    use gitez_agent::Finished;

    use super::*;

    fn tool(name: &str, detail: &str) -> AgentEvent {
        AgentEvent::Tool {
            name: name.to_owned(),
            detail: detail.to_owned(),
        }
    }

    fn text(text: &str) -> AgentEvent {
        AgentEvent::Text(text.to_owned())
    }

    fn finished(text: &str) -> AgentEvent {
        AgentEvent::Finished(Finished {
            success: true,
            text: text.to_owned(),
            session_id: None,
            cost_usd: None,
            turns: None,
            duration_ms: None,
            usage_limit: None,
        })
    }

    fn turn(
        kind: TurnKind,
        prompt: &'static str,
        events: &'static [AgentEvent],
    ) -> TurnEvents<'static> {
        TurnEvents {
            kind,
            prompt,
            events,
        }
    }

    fn leak(events: Vec<AgentEvent>) -> &'static [AgentEvent] {
        Vec::leak(events)
    }

    #[test]
    fn later_prompts_are_entries_and_the_final_message_is_not() {
        let first = leak(vec![
            AgentEvent::Started {
                session_id: Some("s".into()),
                model: None,
            },
            text("Looking."),
            tool("Read", "a.rs"),
            text("Done."),
            finished("Done."),
        ]);
        let second = leak(vec![tool("Edit", "a.rs"), finished("Edited.")]);
        let third = leak(vec![text("Carrying on.")]);
        let entries = entries(&[
            turn(TurnKind::Start, "Fix it", first),
            turn(TurnKind::Reply, "And the tests", second),
            turn(TurnKind::Retry, "GitEZ stopped…", third),
        ]);
        assert_eq!(
            entries,
            [
                Entry::Text("Looking.".into()),
                Entry::Tool {
                    name: "Read".into(),
                    detail: "a.rs".into(),
                    failure: None
                },
                Entry::Prompt {
                    kind: TurnKind::Reply,
                    text: "And the tests".into(),
                    turn: 1,
                },
                Entry::Tool {
                    name: "Edit".into(),
                    detail: "a.rs".into(),
                    failure: None
                },
                Entry::Continued(TurnKind::Retry),
                Entry::Text("Carrying on.".into()),
            ]
        );
    }

    #[test]
    fn a_failure_belongs_to_the_tool_before_it() {
        let events = leak(vec![
            tool("Bash", "cargo test"),
            AgentEvent::ToolFailed("test x failed".into()),
            AgentEvent::ToolFailed("a second line".into()),
            text("Fixing."),
        ]);
        let entries = entries(&[turn(TurnKind::Start, "", events)]);
        assert_eq!(
            entries[0],
            Entry::Tool {
                name: "Bash".into(),
                detail: "cargo test".into(),
                failure: Some("test x failed".into()),
            }
        );
        assert_eq!(entries.len(), 2);
    }

    fn tools(names: &[&str]) -> Vec<Entry> {
        names
            .iter()
            .map(|name| Entry::Tool {
                name: (*name).to_owned(),
                detail: String::new(),
                failure: None,
            })
            .collect()
    }

    #[test]
    fn a_long_run_of_tools_collapses_after_the_first_two() {
        let mut entries = vec![Entry::Text("Start".into())];
        entries.extend(tools(&["Read", "Read", "Grep", "Read", "Grep", "Bash"]));
        entries.push(Entry::Text("End".into()));
        let rows = rows(&entries, &HashSet::new(), false);
        assert_eq!(rows.len(), 5, "{rows:?}");
        assert_eq!(
            rows[3],
            Row::More {
                group: 3,
                count: 4,
                names: "Grep, Read, Bash".into()
            }
        );
        assert!(matches!(rows[4], Row::Text("End")));

        // Expanded, every step shows.
        let all = super::rows(&entries, &HashSet::from([3]), false);
        assert_eq!(all.len(), 8);
        assert!(!all.iter().any(|row| matches!(row, Row::More { .. })));
    }

    #[test]
    fn short_runs_stay_whole() {
        let entries = tools(&["Read", "Read", "Grep"]);
        let rows = rows(&entries, &HashSet::new(), false);
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn a_failed_tool_breaks_the_run_and_shows() {
        let mut entries = tools(&["Read", "Read"]);
        entries.push(Entry::Tool {
            name: "Bash".into(),
            detail: "cargo test".into(),
            failure: Some("failed".into()),
        });
        entries.extend(tools(&["Edit", "Edit", "Edit", "Edit"]));
        let rows = rows(&entries, &HashSet::new(), false);
        assert!(matches!(
            rows[2],
            Row::Tool {
                failure: Some("failed"),
                ..
            }
        ));
        assert_eq!(
            rows[5],
            Row::More {
                group: 5,
                count: 2,
                names: "Edit".into()
            }
        );
        assert_eq!(rows.len(), 6);
    }

    #[test]
    fn the_live_step_is_never_collapsed() {
        let entries = tools(&["Read", "Read", "Grep", "Grep", "Bash"]);
        let rows = rows(&entries, &HashSet::new(), true);
        assert_eq!(
            rows,
            [
                Row::Tool {
                    name: "Read",
                    detail: "",
                    failure: None,
                    live: false
                },
                Row::Tool {
                    name: "Read",
                    detail: "",
                    failure: None,
                    live: false
                },
                Row::More {
                    group: 2,
                    count: 2,
                    names: "Grep".into()
                },
                Row::Tool {
                    name: "Bash",
                    detail: "",
                    failure: None,
                    live: true
                },
            ]
        );
    }

    #[test]
    fn many_tool_names_are_cut_short() {
        let entries = tools(&["A", "B", "C", "D", "E", "F"]);
        let rows = rows(&entries, &HashSet::new(), false);
        assert_eq!(
            rows[2],
            Row::More {
                group: 2,
                count: 4,
                names: "C, D, E, …".into()
            }
        );
    }
}
