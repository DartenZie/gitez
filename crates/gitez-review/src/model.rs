//! What a local pull request holds, as stored in its `review.json`.

use std::collections::BTreeMap;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// A branch reviewed like a pull request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    /// Unique within the repository; shown as `#3`.
    pub number: u32,
    pub title: String,
    pub description: String,
    pub state: State,
    /// The branch the changes are reviewed against.
    pub base: Tip,
    /// The branch under review.
    pub head: Tip,
    pub created: Timestamp,
    pub updated: Timestamp,
    /// The files marked viewed, by path, with the blob id each had then: a
    /// file whose content changed since counts as not viewed.
    pub viewed: BTreeMap<String, String>,
    pub prompt: PromptOptions,
    pub threads: Vec<Thread>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Open,
    Merged,
    Closed,
}

/// A branch and the last commit GitEZ saw on it. When the branch has moved
/// on, the threads are checked against the new commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tip {
    pub branch: String,
    /// Full hexadecimal id.
    pub commit: String,
}

/// How the agent prompt is put together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptOptions {
    /// Which agent "Send" runs: `claude-code`, or `none` to only copy.
    pub agent: String,
    pub template: Template,
    /// Quote the commented lines under each comment.
    pub quote_code: bool,
    /// Ask the agent to run the tests when it is done.
    pub run_tests: bool,
}

impl Default for PromptOptions {
    fn default() -> Self {
        Self {
            agent: "claude-code".to_owned(),
            template: Template::Fix,
            quote_code: true,
            run_tests: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Template {
    /// Change the code as each comment asks.
    Fix,
    /// Explain the code each comment asks about, without changing it.
    Explain,
}

/// Comments on a range of lines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thread {
    /// Unique within the pull request.
    pub id: u32,
    pub anchor: Anchor,
    /// Whether the agent prompt includes the thread.
    pub in_prompt: bool,
    /// `None` while the thread is open.
    pub resolved: Option<Resolution>,
    pub comments: Vec<Comment>,
}

impl Thread {
    pub fn is_open(&self) -> bool {
        self.resolved.is_none()
    }
}

/// Where a thread points: lines of one side of a file's diff at a commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    /// The head commit the line numbers refer to (full hexadecimal id).
    pub commit: String,
    /// The file's path from the repository root, on that side.
    pub path: String,
    pub side: Side,
    /// First and last line, 1-based and inclusive.
    pub lines: (u32, u32),
    /// The lines themselves, without line endings: quoted in the prompt,
    /// and kept if the file later changes.
    pub excerpt: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// The base's version of the file: removed lines.
    Old,
    /// The head's version of the file: added and unchanged lines.
    New,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolution {
    pub by: ResolvedBy,
    /// The head commit when it was resolved.
    pub commit: String,
    pub at: Timestamp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedBy {
    /// A new commit changed the lines the thread points at.
    Auto,
    /// The reviewer resolved it.
    You,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comment {
    /// Unique within the pull request.
    pub id: u32,
    pub body: String,
    pub created: Timestamp,
}

impl PullRequest {
    /// Starts a thread at `anchor` with the comment `body`, included in the
    /// agent prompt. Returns the thread's id.
    pub fn add_thread(&mut self, anchor: Anchor, body: String, at: Timestamp) -> u32 {
        let id = self
            .threads
            .iter()
            .map(|thread| thread.id)
            .max()
            .unwrap_or(0)
            + 1;
        let comment = self.new_comment(body, at);
        self.threads.push(Thread {
            id,
            anchor,
            in_prompt: true,
            resolved: None,
            comments: vec![comment],
        });
        self.updated = at;
        id
    }

    /// Adds the comment `body` to thread `thread`. Returns the comment's id.
    pub fn reply(&mut self, thread: u32, body: String, at: Timestamp) -> Result<u32> {
        let comment = self.new_comment(body, at);
        let id = comment.id;
        self.thread_mut(thread)?.comments.push(comment);
        self.updated = at;
        Ok(id)
    }

    /// Resolves thread `thread`, or reopens it with `None`.
    pub fn set_resolution(&mut self, thread: u32, resolution: Option<Resolution>) -> Result<()> {
        if let Some(resolution) = &resolution {
            self.updated = resolution.at;
        }
        self.thread_mut(thread)?.resolved = resolution;
        Ok(())
    }

    /// Includes thread `thread` in the agent prompt, or leaves it out.
    pub fn set_in_prompt(&mut self, thread: u32, in_prompt: bool) -> Result<()> {
        self.thread_mut(thread)?.in_prompt = in_prompt;
        Ok(())
    }

    /// How many threads are not resolved.
    pub fn open_threads(&self) -> usize {
        self.threads
            .iter()
            .filter(|thread| thread.is_open())
            .count()
    }

    pub fn thread(&self, thread: u32) -> Option<&Thread> {
        self.threads.iter().find(|candidate| candidate.id == thread)
    }

    fn thread_mut(&mut self, thread: u32) -> Result<&mut Thread> {
        let number = self.number;
        self.threads
            .iter_mut()
            .find(|candidate| candidate.id == thread)
            .ok_or(Error::NoThread { number, thread })
    }

    fn new_comment(&self, body: String, at: Timestamp) -> Comment {
        let id = self
            .threads
            .iter()
            .flat_map(|thread| &thread.comments)
            .map(|comment| comment.id)
            .max()
            .unwrap_or(0)
            + 1;
        Comment {
            id,
            body,
            created: at,
        }
    }
}
