//! The supervisor's pipeline run in-process with a fake `claude` and no
//! sandbox (DESIGN G, P6 acceptance tests 1-4, 6, 7, 11).
#![cfg(unix)]

mod common;

use std::time::Duration;

use common::{Fixture, branch_of, git, worktree_of};
use gitez_agent_thread::{
    BRIEF_PROMPT, CONTINUE_PROMPT, Ending, HANDOFF_MARKER, Handoff, Mode, RetryPlan, State, Step,
    Timestamp, TurnKind,
};
use gitez_supervisor::{Command, Error};

const PASSTHROUGH_WARNING: &str = "Ran without a sandbox (GITEZ_TEST_SANDBOX)";

/// The session id the fake was given in run `n` (0-based), and whether it
/// was resumed.
fn session_of(call: &str) -> (String, bool) {
    let words: Vec<&str> = call.split(' ').collect();
    for (index, word) in words.iter().enumerate() {
        if *word == "--session-id" || *word == "--resume" {
            return (words[index + 1].to_owned(), *word == "--resume");
        }
    }
    panic!("no session in {call}");
}

#[test]
fn start_creates_the_worktree_runs_setup_and_the_agent_and_opens_a_pull_request() {
    let fixture = Fixture::new();
    fixture.set_setup_command("touch setup-ran");
    let start = fixture.head();
    let created = fixture.create("COMMIT a file for me");
    let id = created.id.clone();

    let thread = fixture.run(&id, Command::Start);

    assert_eq!(thread.state, State::Finished);
    assert_eq!(thread.ending, Some(Ending::PullRequest), "{thread:#?}");
    assert!(thread.supervisor.is_none() && thread.activity.is_none());
    assert!(thread.prepared);
    assert!(thread.warnings.iter().any(|w| w == PASSTHROUGH_WARNING));
    // The worktree, on the thread's branch, prepared by the setup command.
    assert!(worktree_of(&thread).join(".git").is_file());
    assert!(worktree_of(&thread).join("setup-ran").is_file());
    assert_eq!(
        git(worktree_of(&thread), &["branch", "--show-current"]),
        branch_of(&thread)
    );
    assert_eq!(branch_of(&thread), format!("agent/{id}"));
    // The agent's commit is on its branch; main did not move.
    assert_eq!(
        git(
            &fixture.root,
            &["log", "-1", "--format=%s", branch_of(&thread)]
        ),
        "Agent: first line"
    );
    assert_eq!(fixture.tip("main"), start);
    // The session id GitEZ made is the one stored.
    let calls = fixture.calls();
    assert_eq!(calls.len(), 1);
    let (session, resumed) = session_of(&calls[0]);
    assert!(!resumed);
    assert_eq!(thread.session_id.as_deref(), Some(session.as_str()));
    assert!(calls[0].contains("--model haiku"));
    assert!(calls[0].contains("--effort low"));
    assert!(calls[0].contains("--append-system-prompt You are working in a git worktree"));
    // The transcript: GitEZ's header, then the agent's lines.
    let transcripts = fixture.transcripts(&id);
    assert_eq!(transcripts.len(), 1);
    let (header, lines) = &transcripts[0];
    assert_eq!(header.prompt, "COMMIT a file for me");
    assert_eq!(header.kind, TurnKind::Start);
    assert_eq!(header.session_id.as_deref(), Some(session.as_str()));
    assert_eq!(lines.len(), 4, "{lines:#?}");
    assert!(lines[0].contains(r#""subtype":"init""#));
    let turn = &thread.turns[0];
    assert_eq!(
        turn.transcript.as_deref(),
        fixture.store().runs(&id).expect("runs")[0]
            .file_name()
            .and_then(|name| name.to_str())
    );
    let result = turn.result.as_ref().expect("a result");
    assert!(result.success);
    assert_eq!(result.steps, 1);
    assert_eq!(result.message, "Wrote agent.txt and committed it.");
    // The local pull request.
    let reviews = gitez_review::Store::new(&fixture.common_dir());
    assert_eq!(reviews.numbers().expect("numbers"), [1]);
    assert_eq!(thread.pull_request, Some(1));
    let pull_request = reviews.load(1).expect("load");
    // No heading in the final message: the agent's commit names it, and
    // the thread takes the name; its id and branch stay.
    assert_eq!(pull_request.title, thread.title);
    assert_eq!(pull_request.title, "Agent: first line");
    assert_eq!(thread.id, id);
    assert_eq!(
        pull_request.description,
        "Wrote agent.txt and committed it."
    );
    assert_eq!(pull_request.base.branch, "main");
    assert_eq!(pull_request.base.commit, start);
    assert_eq!(pull_request.head.branch, branch_of(&thread));
    assert_eq!(pull_request.head.commit, fixture.tip(branch_of(&thread)));
}

#[test]
fn the_heading_of_the_final_message_names_the_pull_request_and_the_thread() {
    let fixture = Fixture::new();
    let created = fixture.create("TITLED: can you look into the greeting and make it friendlier?");
    assert_eq!(created.title, "TITLED");
    let id = created.id.clone();
    let thread = fixture.run(&id, Command::Start);
    assert_eq!(thread.ending, Some(Ending::PullRequest), "{thread:#?}");
    let pull_request = gitez_review::Store::new(&fixture.common_dir())
        .load(1)
        .expect("load");
    assert_eq!(pull_request.title, "Make the greeting friendlier");
    assert_eq!(pull_request.description, "The greeting is warmer now.");
    // The thread takes the same name; its id, branch and worktree stay.
    assert_eq!(thread.title, "Make the greeting friendlier");
    assert_eq!(thread.id, id);
    assert_eq!(branch_of(&thread), branch_of(&created));
    assert_eq!(worktree_of(&thread), worktree_of(&created));
    assert_eq!(fixture.load(&id).title, "Make the greeting friendlier");
    // The final message keeps its heading; it is shown apart.
    assert_eq!(
        thread.final_message(),
        Some("# Make the greeting friendlier\n\nThe greeting is warmer now.")
    );
    assert_eq!(
        thread.final_message_body(),
        Some("The greeting is warmer now.")
    );
    // GitEZ's commit of the leftovers already uses the agent's title.
    assert_eq!(
        git(
            &fixture.root,
            &["log", "-1", "--format=%B", branch_of(&thread)]
        ),
        "Make the greeting friendlier\n\nChanges the agent left uncommitted, committed by GitEZ."
    );
}

#[test]
fn without_a_heading_the_oldest_commit_names_the_pull_request() {
    let fixture = Fixture::new();
    let id = fixture.create("TWO-COMMITS please").id;
    let thread = fixture.run(&id, Command::Start);
    assert_eq!(thread.ending, Some(Ending::PullRequest), "{thread:#?}");
    // Cleaned: no quotes, no trailing period, no surrounding spaces.
    assert_eq!(thread.title, "Add one.txt");
    let pull_request = gitez_review::Store::new(&fixture.common_dir())
        .load(1)
        .expect("load");
    assert_eq!(pull_request.title, "Add one.txt");
    assert_eq!(pull_request.description, "Added two files.");
}

#[test]
fn a_later_turn_does_not_rename_the_pull_request() {
    let fixture = Fixture::new();
    let id = fixture.create("COMMIT a file for me").id;
    let first = fixture.run(&id, Command::Start);
    assert_eq!(first.title, "Agent: first line");
    fixture
        .store()
        .update(&id, |thread| {
            thread.request_turn(
                TurnKind::Reply,
                "RETITLE and add a third line".to_owned(),
                Timestamp::now(),
            )
        })
        .expect("reply");
    let thread = fixture.run(&id, Command::Reply);
    assert_eq!(thread.ending, Some(Ending::PullRequest), "{thread:#?}");
    assert_eq!(thread.title, "Agent: first line");
    let pull_request = gitez_review::Store::new(&fixture.common_dir())
        .load(1)
        .expect("load");
    assert_eq!(pull_request.title, "Agent: first line");
    assert_eq!(
        pull_request.description,
        "Wrote agent.txt and committed it."
    );
    // The new heading is not the thread's title, so it stays in the
    // message shown.
    assert_eq!(
        thread.final_message_body(),
        Some("# Rename everything\n\nAdded a third line.")
    );
}

#[test]
fn an_answer_without_changes_opens_no_pull_request() {
    let fixture = Fixture::new();
    let id = fixture.create("What is the answer?").id;
    let thread = fixture.run(&id, Command::Start);
    assert_eq!(thread.ending, Some(Ending::Answer), "{thread:#?}");
    assert_eq!(thread.pull_request, None);
    assert_eq!(thread.final_message(), Some("The answer is 42."));
    assert_eq!(fixture.tip(branch_of(&thread)), thread.start.commit);
    let reviews = gitez_review::Store::new(&fixture.common_dir());
    assert!(reviews.numbers().expect("numbers").is_empty());
}

#[test]
fn changes_the_agent_left_are_committed_by_gitez() {
    let fixture = Fixture::new();
    let id = fixture.create("LEAVE a file uncommitted").id;
    let thread = fixture.run(&id, Command::Start);
    assert_eq!(thread.ending, Some(Ending::PullRequest), "{thread:#?}");
    assert_eq!(
        git(
            &fixture.root,
            &["log", "-1", "--format=%B", branch_of(&thread)]
        ),
        format!(
            "{}\n\nChanges the agent left uncommitted, committed by GitEZ.",
            thread.title
        )
    );
    assert_eq!(
        git(
            &fixture.root,
            &["show", &format!("{}:left.txt", branch_of(&thread))]
        ),
        "left"
    );
    assert_eq!(git(worktree_of(&thread), &["status", "--porcelain"]), "");
    assert_eq!(thread.pull_request, Some(1));
    // GitEZ's own commit does not name the pull request: the prompt does.
    assert_eq!(thread.title, "LEAVE a file uncommitted");
    let pull_request = gitez_review::Store::new(&fixture.common_dir())
        .load(1)
        .expect("load");
    assert_eq!(pull_request.title, "LEAVE a file uncommitted");
    assert_eq!(pull_request.description, "Left a file for GitEZ.");
    // The supervisor recorded that nothing is left uncommitted.
    let uncommitted = thread.uncommitted.expect("the worktree was checked");
    assert_eq!(uncommitted.count, 0);
    assert!(uncommitted.commit_error.is_none());
}

/// The GUI-critical finding, library half: when GitEZ's commit of the
/// leftovers fails (a failing pre-commit hook), the changes stay in the
/// worktree and the thread records them — never "No changes".
#[test]
fn a_failed_leftover_commit_is_recorded_not_hidden() {
    let fixture = Fixture::new();
    // A hook that refuses the leftover commit. It is on the common dir, so
    // the worktree's commit uses it too.
    let hooks = fixture.base.join("no hooks");
    let pre_commit = hooks.join("pre-commit");
    std::fs::write(
        &pre_commit,
        "#!/bin/sh\necho 'no commits today' >&2\nexit 1\n",
    )
    .expect("write the hook");
    std::fs::set_permissions(
        &pre_commit,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .expect("chmod");

    let id = fixture.create("LEAVE a file uncommitted").id;
    let thread = fixture.run(&id, Command::Start);
    // The run itself did not fail; the leftover commit did.
    assert_ne!(thread.ending, Some(Ending::Failed), "{thread:#?}");
    let uncommitted = thread
        .uncommitted
        .clone()
        .expect("the worktree was checked");
    assert!(
        uncommitted.count > 0,
        "the change is still there: {uncommitted:?}"
    );
    assert!(
        uncommitted
            .paths
            .iter()
            .any(|path| path.contains("left.txt")),
        "{uncommitted:?}"
    );
    let error = uncommitted.commit_error.expect("a recorded failure");
    assert!(
        error.contains("hook") || error.contains("no commits today"),
        "{error}"
    );
    assert!(thread.has_uncommitted() == Some(true));
    assert!(thread.leftover_commit_failed());
    // The change really is still in the worktree.
    assert!(!git(worktree_of(&thread), &["status", "--porcelain"]).is_empty());
    assert!(
        thread
            .warnings
            .iter()
            .any(|w| w.contains("could not commit")),
        "{:?}",
        thread.warnings
    );
}

#[test]
fn a_failed_run_is_retried_in_a_new_conversation_when_the_agent_never_started() {
    let fixture = Fixture::new();
    let id = fixture.create("FAIL-NO-INIT please").id;
    let thread = fixture.run(&id, Command::Start);
    assert_eq!(thread.ending, Some(Ending::Failed));
    let error = thread.error.as_ref().expect("an error");
    assert_eq!(error.step, Step::Agent);
    assert!(
        error.message.contains("boom: the agent fell over"),
        "{error:?}"
    );
    assert_eq!(thread.session_id, None);
    assert!(thread.turns[0].ended.is_some());

    let plan = fixture
        .store()
        .update(&id, |thread| {
            thread.request_retry(thread.network, Timestamp::now())
        })
        .expect("retry");
    assert_eq!(plan, RetryPlan::RunAgain);
    let thread = fixture.run(&id, Command::Retry);
    assert_eq!(thread.ending, Some(Ending::Answer), "{thread:#?}");
    assert!(thread.error.is_none());
    let calls = fixture.calls();
    assert_eq!(calls.len(), 2);
    let (first, _) = session_of(&calls[0]);
    let (second, resumed) = session_of(&calls[1]);
    assert!(!resumed);
    assert_ne!(first, second);
    assert_eq!(thread.session_id.as_deref(), Some(second.as_str()));
    assert_eq!(thread.turns[1].kind, TurnKind::Retry);
    assert_eq!(thread.turns[1].prompt, "FAIL-NO-INIT please");
}

#[test]
fn a_failed_run_is_continued_when_the_agent_had_started() {
    let fixture = Fixture::new();
    let id = fixture.create("FAIL-AFTER-INIT please").id;
    let thread = fixture.run(&id, Command::Start);
    assert_eq!(thread.ending, Some(Ending::Failed));
    let error = thread.error.as_ref().expect("an error");
    assert_eq!(error.step, Step::Agent);
    assert!(error.message.contains("boom after init"), "{error:?}");
    let session = thread.session_id.clone().expect("the agent started");

    let plan = fixture
        .store()
        .update(&id, |thread| {
            thread.request_retry(thread.network, Timestamp::now())
        })
        .expect("retry");
    assert_eq!(plan, RetryPlan::Continue);
    let thread = fixture.run(&id, Command::Retry);
    assert_eq!(thread.ending, Some(Ending::Answer), "{thread:#?}");
    let calls = fixture.calls();
    assert_eq!(session_of(&calls[1]), (session, true));
    assert_eq!(thread.turns[1].prompt, CONTINUE_PROMPT);
}

#[test]
fn a_usage_limit_waits_for_the_reset_then_resumes() {
    let fixture = Fixture::new();
    let id = fixture.create("LIMIT me").id;
    let environment = fixture.environment();
    let common_dir = fixture.common_dir();
    let run_id = id.clone();
    let runner = std::thread::spawn(move || {
        gitez_supervisor::run(
            &environment,
            &common_dir,
            &run_id,
            Command::Start,
            Default::default(),
        )
    });
    let waiting = fixture.wait_for(&id, Duration::from_secs(20), |thread| {
        thread.state == State::Waiting || !thread.state.is_active()
    });
    assert_eq!(waiting.state, State::Waiting, "{waiting:#?}");
    let until = waiting.waiting_until.expect("waiting until");
    assert!(until > Timestamp::now());

    let thread = runner.join().expect("join").expect("run");
    assert_eq!(thread.ending, Some(Ending::Answer), "{thread:#?}");
    assert!(thread.waiting_until.is_none());
    assert_eq!(thread.turns.len(), 2);
    let resume = &thread.turns[1];
    assert_eq!(resume.kind, TurnKind::Resume);
    assert!(resume.resume);
    assert_eq!(resume.prompt, CONTINUE_PROMPT);
    assert!(resume.started.expect("started") >= until);
    let calls = fixture.calls();
    assert_eq!(calls.len(), 2);
    let session = thread.session_id.clone().expect("session");
    assert_eq!(session_of(&calls[1]), (session, true));
    // The resume prompt is CONTINUE_PROMPT, which the fake answers.
    assert_eq!(thread.final_message(), Some("The answer is 42."));
}

#[test]
fn a_reply_resumes_the_conversation_and_updates_the_same_pull_request() {
    let fixture = Fixture::new();
    let id = fixture.create("COMMIT a file for me").id;
    let first = fixture.run(&id, Command::Start);
    assert_eq!(first.pull_request, Some(1));
    let session = first.session_id.clone().expect("session");

    fixture
        .store()
        .update(&id, |thread| {
            thread.request_turn(
                TurnKind::Reply,
                "COMMIT-AGAIN also add a second line".to_owned(),
                Timestamp::now(),
            )
        })
        .expect("reply");
    let thread = fixture.run(&id, Command::Reply);
    assert_eq!(thread.ending, Some(Ending::PullRequest), "{thread:#?}");
    assert_eq!(thread.pull_request, Some(1));
    let calls = fixture.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(session_of(&calls[1]), (session, true));
    let reviews = gitez_review::Store::new(&fixture.common_dir());
    assert_eq!(reviews.numbers().expect("numbers"), [1]);
    assert_eq!(
        git(
            &fixture.root,
            &[
                "rev-list",
                "--count",
                &format!("main..{}", branch_of(&thread))
            ]
        ),
        "2"
    );
    assert_eq!(fixture.transcripts(&id).len(), 2);
    assert_eq!(thread.final_message(), Some("Added a second line."));
}

#[test]
fn another_branch_moving_is_a_warning_not_a_failure() {
    let fixture = Fixture::new();
    let id = fixture.create("MOVE something").id;
    let thread = fixture.run(&id, Command::Start);
    assert_eq!(thread.ending, Some(Ending::Answer), "{thread:#?}");
    assert!(
        thread.warnings.iter().any(|warning| warning
            == "Moved while the agent ran: other. If you did not move them, check what the agent did."),
        "{:#?}",
        thread.warnings
    );
}

#[test]
fn a_subcommand_that_does_not_match_fails_the_thread() {
    let fixture = Fixture::new();
    let id = fixture.create("What is the answer?").id;
    let error = gitez_supervisor::run(
        &fixture.environment(),
        &fixture.common_dir(),
        &id,
        Command::Reply,
        Default::default(),
    )
    .expect_err("a start is pending, not a reply");
    assert!(matches!(error, Error::Usage(_)), "{error:?}");
    let thread = fixture.load(&id);
    assert_eq!(thread.ending, Some(Ending::Failed));
    assert_eq!(thread.error.expect("error").step, Step::Supervisor);
    assert!(fixture.calls().is_empty());
}

#[test]
fn a_missing_thread_is_an_error() {
    let fixture = Fixture::new();
    let id = gitez_agent_thread::AgentThreadId::new("nothing-here").expect("id");
    let error = gitez_supervisor::run(
        &fixture.environment(),
        &fixture.common_dir(),
        &id,
        Command::Start,
        Default::default(),
    )
    .expect_err("no such thread");
    assert!(matches!(error, Error::Thread(_)), "{error:?}");
}

/// The branches and worktrees of the fixture's repository, as git lists
/// them.
fn branches_and_worktrees(fixture: &Fixture) -> (String, String) {
    (
        git(
            &fixture.root,
            &["branch", "--list", "--format=%(refname:short)"],
        ),
        git(&fixture.root, &["worktree", "list", "--porcelain"]),
    )
}

#[test]
fn a_discuss_thread_runs_in_the_working_tree_and_makes_nothing() {
    let fixture = Fixture::new();
    // Not run: the working tree is the user's, as it is.
    fixture.set_setup_command("touch setup-ran");
    std::fs::write(fixture.root.join("mine.txt"), "uncommitted\n").expect("write");
    let before = branches_and_worktrees(&fixture);
    let created = fixture.discuss("WHERE are you? I want to talk about the cache.");
    assert_eq!(created.mode, Mode::Discuss);

    let thread = fixture.run(&created.id, Command::Start);
    assert_eq!(thread.state, State::Finished, "{thread:#?}");
    assert_eq!(thread.ending, Some(Ending::Answer));
    assert_eq!(thread.pull_request, None);
    assert_eq!(
        (thread.branch.as_ref(), thread.worktree.as_ref()),
        (None, None)
    );
    assert!(!thread.prepared);
    assert_eq!(thread.worktree_record, None);
    assert_eq!(thread.uncommitted, None, "GitEZ did not look for leftovers");

    // The agent ran in the working tree itself, with a temporary folder of
    // its own, and git told not to touch the index.
    let scratch = thread.scratch_dirs().expect("scratch folders");
    assert_eq!(
        thread.final_message(),
        Some(
            format!(
                "cwd={} locks=0 tmp={}",
                fixture.root.display(),
                scratch.tmp.display()
            )
            .as_str()
        )
    );
    assert!(scratch.tmp.starts_with(fixture.data_dir.join("tmp")));
    assert!(scratch.tmp.is_dir());
    let calls = fixture.calls();
    assert_eq!(calls.len(), 1);
    assert!(
        calls[0].contains("--append-system-prompt You are in a discussion"),
        "{}",
        calls[0]
    );
    assert!(!calls[0].contains("git worktree that GitEZ created"));

    // Nothing was made: no branch, no worktree, no pull request, no setup.
    assert_eq!(branches_and_worktrees(&fixture), before);
    assert!(!fixture.data_dir.join("worktrees").exists());
    assert!(!fixture.root.join("setup-ran").exists());
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("mine.txt")).expect("read"),
        "uncommitted\n"
    );
    let reviews = gitez_review::Store::new(&fixture.common_dir());
    assert!(reviews.numbers().expect("numbers").is_empty());
}

#[test]
fn gitez_commits_nothing_in_the_working_tree_of_a_discuss_thread() {
    // Without a sandbox the fake agent can write where the real one cannot;
    // GitEZ still commits nothing there and opens no pull request.
    let fixture = Fixture::new();
    let head = fixture.head();
    let id = fixture.discuss("LEAVE a file uncommitted").id;
    let thread = fixture.run(&id, Command::Start);
    assert_eq!(thread.ending, Some(Ending::Answer), "{thread:#?}");
    assert_eq!(thread.pull_request, None);
    assert_eq!(thread.uncommitted, None);
    assert_eq!(fixture.head(), head);
    assert_eq!(
        git(&fixture.root, &["status", "--porcelain"]),
        "?? left.txt"
    );

    // A commit the agent made itself is the user's branch moving: said,
    // and still no pull request.
    let id = fixture.discuss("COMMIT a file for me").id;
    let thread = fixture.run(&id, Command::Start);
    assert_eq!(thread.ending, Some(Ending::Answer), "{thread:#?}");
    assert_eq!(thread.pull_request, None);
    assert!(
        thread
            .warnings
            .iter()
            .any(|warning| warning.starts_with("Moved while the agent ran: main")),
        "{:#?}",
        thread.warnings
    );
    let reviews = gitez_review::Store::new(&fixture.common_dir());
    assert!(reviews.numbers().expect("numbers").is_empty());
}

#[test]
fn a_reply_and_a_brief_continue_a_discussion_in_the_same_folder() {
    let fixture = Fixture::new();
    let id = fixture.discuss("What is the answer?").id;
    let first = fixture.run(&id, Command::Start);
    let session = first.session_id.clone().expect("session");
    assert_eq!(first.brief(), None);

    fixture
        .store()
        .update(&id, |thread| {
            thread.request_turn(
                TurnKind::Reply,
                "WHERE are you now?".to_owned(),
                Timestamp::now(),
            )
        })
        .expect("reply");
    let replied = fixture.run(&id, Command::Reply);
    assert_eq!(replied.ending, Some(Ending::Answer), "{replied:#?}");
    assert!(
        replied.final_message().is_some_and(
            |message| message.starts_with(&format!("cwd={} locks=0", fixture.root.display()))
        ),
        "{:?}",
        replied.final_message()
    );

    fixture
        .store()
        .update(&id, |thread| {
            thread.request_turn(TurnKind::Brief, BRIEF_PROMPT.to_owned(), Timestamp::now())
        })
        .expect("brief");
    let briefed = fixture.run(&id, Command::Reply);
    assert_eq!(briefed.ending, Some(Ending::Answer), "{briefed:#?}");
    assert_eq!(briefed.brief(), Some("The answer is 42."));
    assert_eq!(briefed.turns.len(), 3);
    assert_eq!(briefed.turns[2].kind, TurnKind::Brief);

    let calls = fixture.calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(session_of(&calls[0]), (session.clone(), false));
    assert_eq!(session_of(&calls[1]), (session.clone(), true));
    assert_eq!(session_of(&calls[2]), (session, true));
    assert_eq!(fixture.transcripts(&id).len(), 3);
    assert_eq!(fixture.stdin(3), BRIEF_PROMPT);
    assert!(!fixture.data_dir.join("worktrees").exists());
}

#[test]
fn a_discuss_thread_whose_working_tree_is_gone_fails_before_the_agent() {
    let fixture = Fixture::new();
    let id = fixture.discuss("What is the answer?").id;
    let elsewhere = fixture.base.join("gone");
    fixture
        .store()
        .update(&id, |thread| {
            thread.work_dir = Some(elsewhere.clone());
            Ok(())
        })
        .expect("update");
    let thread = fixture.run(&id, Command::Start);
    assert_eq!(thread.ending, Some(Ending::Failed), "{thread:#?}");
    let error = thread.error.expect("an error");
    assert_eq!(error.step, Step::Worktree);
    assert_eq!(
        error.message,
        format!(
            "The working tree {} is gone, or is no longer one of this repository.",
            elsewhere.display()
        )
    );
    assert!(fixture.calls().is_empty(), "the agent did not run");

    // Another repository's working tree is not this thread's either.
    let other = fixture.base.join("other repo");
    std::fs::create_dir_all(&other).expect("create");
    git(&other, &["init", "--quiet", "--initial-branch=main"]);
    let id = fixture.discuss("What is the answer, again?").id;
    fixture
        .store()
        .update(&id, |thread| {
            thread.work_dir = Some(other.clone());
            Ok(())
        })
        .expect("update");
    let thread = fixture.run(&id, Command::Start);
    assert_eq!(thread.ending, Some(Ending::Failed), "{thread:#?}");
    assert_eq!(thread.error.expect("an error").step, Step::Worktree);
    assert!(fixture.calls().is_empty());
}

#[test]
fn an_auto_thread_records_a_marked_answer_as_a_brief_without_the_marker() {
    let fixture = Fixture::new();
    let before = branches_and_worktrees(&fixture);
    let created = fixture.auto("HANDOFF: follow renames in blame");
    assert_eq!((created.mode, created.auto), (Mode::Discuss, true));

    let thread = fixture.run(&created.id, Command::Start);
    // Still a discussion: an answer, nothing made.
    assert_eq!(thread.state, State::Finished, "{thread:#?}");
    assert_eq!(thread.ending, Some(Ending::Answer));
    assert_eq!(branches_and_worktrees(&fixture), before);
    let brief = "# Follow renames in blame\n\nRead the rename from the diff of each commit.";
    assert_eq!(thread.brief(), Some(brief));
    assert_eq!(thread.final_message(), Some(brief));
    assert!(
        !thread
            .final_message()
            .unwrap_or_default()
            .contains(HANDOFF_MARKER)
    );
    assert_eq!(thread.turns[0].handoff, Some(Handoff::default()));
    assert_eq!(thread.pending_handoff(), Some(0));

    // The agent was told how to hand off.
    let calls = fixture.calls();
    assert!(calls[0].contains(HANDOFF_MARKER), "{}", calls[0]);
}

#[test]
fn an_auto_thread_whose_agent_answers_hands_nothing_off() {
    let fixture = Fixture::new();
    let id = fixture.auto("What is the answer?").id;
    let thread = fixture.run(&id, Command::Start);
    assert_eq!(thread.ending, Some(Ending::Answer), "{thread:#?}");
    assert_eq!(thread.final_message(), Some("The answer is 42."));
    assert_eq!(thread.turns[0].handoff, None);
    assert_eq!(thread.brief(), None);
    assert_eq!(thread.pending_handoff(), None);

    // A later reply can hand off ("ok, do it").
    fixture
        .store()
        .update(&id, |thread| {
            thread.request_turn(
                TurnKind::Reply,
                "HANDOFF: ok, do it".to_owned(),
                Timestamp::now(),
            )
        })
        .expect("reply");
    let replied = fixture.run(&id, Command::Reply);
    assert_eq!(replied.turns[1].handoff, Some(Handoff::default()));
    assert_eq!(replied.pending_handoff(), Some(1));
}

#[test]
fn a_plain_discussion_keeps_a_marker_it_was_never_told_about() {
    let fixture = Fixture::new();
    let id = fixture.discuss("HANDOFF: follow renames in blame").id;
    let thread = fixture.run(&id, Command::Start);
    assert_eq!(thread.turns[0].handoff, None);
    assert_eq!(thread.brief(), None);
    assert!(!fixture.calls()[0].contains(HANDOFF_MARKER));
}
