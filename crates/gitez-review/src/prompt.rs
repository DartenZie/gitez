//! The prompt for a coding agent, built from a pull request's threads, as
//! in the `LocalPR.dc.html` artboard.

use crate::model::{PullRequest, Side, Template, Thread};

/// What the prompt says about the repository, beyond the pull request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptContext {
    /// The repository's name.
    pub repository: String,
    /// How many commits the head branch has that the base lacks, if known.
    pub commits_ahead: Option<usize>,
    /// The command that runs the project's tests, if known.
    pub test_command: Option<String>,
}

impl PullRequest {
    /// The threads the prompt includes, in the order it lists them: open
    /// threads marked for the prompt, by file and line.
    pub fn prompt_threads(&self) -> Vec<&Thread> {
        let mut threads: Vec<&Thread> = self
            .threads
            .iter()
            .filter(|thread| thread.is_open() && thread.in_prompt)
            .collect();
        threads.sort_by(|a, b| {
            let key = |thread: &Thread| {
                let anchor = &thread.anchor;
                (anchor.path.clone(), anchor.lines, anchor.side == Side::Old)
            };
            key(a).cmp(&key(b))
        });
        threads
    }

    /// The prompt for the agent, following the pull request's prompt
    /// options; `None` when no thread is included.
    pub fn prompt(&self, context: &PromptContext) -> Option<String> {
        let threads = self.prompt_threads();
        if threads.is_empty() {
            return None;
        }
        let options = &self.prompt;
        let mut prompt = format!("Repository: {}\n", context.repository);
        prompt.push_str(&match context.commits_ahead {
            Some(1) => format!(
                "Branch: {} (1 commit ahead of {})\n",
                self.head.branch, self.base.branch
            ),
            Some(ahead) => format!(
                "Branch: {} ({ahead} commits ahead of {})\n",
                self.head.branch, self.base.branch
            ),
            None => format!(
                "Branch: {}, reviewed against {}\n",
                self.head.branch, self.base.branch
            ),
        });
        prompt.push('\n');

        let tests = match &context.test_command {
            Some(command) => format!("run `{command}`"),
            None => "run the project's tests".to_owned(),
        };
        prompt.push_str(&match options.template {
            Template::Fix => {
                let mut instructions = "Address the review comments below. Change only what \
                     each comment asks and leave unrelated code as it is. Make one commit per \
                     comment, with a message starting \"review:\"."
                    .to_owned();
                if options.run_tests {
                    instructions.push_str(&format!(" When done, {tests} and report the result."));
                }
                instructions
            }
            Template::Explain => "Answer the review comments below by explaining the code \
                 each one points at. Do not change any files."
                .to_owned(),
        });
        prompt.push('\n');

        for (index, thread) in threads.iter().enumerate() {
            prompt.push('\n');
            let anchor = &thread.anchor;
            let lines = match anchor.lines {
                (first, last) if first == last => format!("line {first}"),
                (first, last) => format!("lines {first}–{last}"),
            };
            let removed = match anchor.side {
                Side::New => "",
                Side::Old => "removed ",
            };
            prompt.push_str(&format!(
                "{}. {}, {removed}{lines}\n",
                index + 1,
                anchor.path
            ));
            if options.quote_code {
                for line in dedent(&anchor.excerpt) {
                    push_line(&mut prompt, "   ", line);
                }
            }
            for comment in &thread.comments {
                let mut body = comment.body.trim().lines();
                push_line(&mut prompt, "   → ", body.next().unwrap_or_default());
                // Further lines line up with the first.
                for line in body {
                    push_line(&mut prompt, "     ", line);
                }
            }
        }
        Some(prompt)
    }
}

/// Adds `text` after `indent` as a line, without trailing whitespace.
fn push_line(prompt: &mut String, indent: &str, text: &str) {
    let line = format!("{indent}{text}");
    prompt.push_str(line.trim_end());
    prompt.push('\n');
}

/// `lines` without the indentation they all share, ignoring blank lines.
fn dedent(lines: &[String]) -> Vec<&str> {
    let indent = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start().len())
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .map(|line| line.get(indent..).unwrap_or("").trim_end())
        .collect()
}
