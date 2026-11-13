//! The supervisor's pipeline run in-process with a fake `claude` and no
//! sandbox (DESIGN G, P6 acceptance tests 1-4, 6, 7, 11).
#![cfg(unix)]

mod common;

use std::time::Duration;

use common::{Fixture, git};
use gitez_agent_thread::{CONTINUE_PROMPT, Ending, RetryPlan, State, Step, Timestamp, TurnKind};
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
    assert!(thread.worktree.join(".git").is_file());
    assert!(thread.worktree.join("setup-ran").is_file());
    assert_eq!(
        git(&thread.worktree, &["branch", "--show-current"]),
        thread.branch
    );
    assert_eq!(thread.branch, format!("agent/{id}"));
    // The agent's commit is on its branch; main did not move.
    assert_eq!(
        git(&fixture.root, &["log", "-1", "--format=%s", &thread.branch]),
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
    assert_eq!(pull_request.title, thread.title);
    assert_eq!(pull_request.title, "COMMIT a file for me");
    assert_eq!(
        pull_request.description,
        "Wrote agent.txt and committed it."
    );
    assert_eq!(pull_request.base.branch, "main");
    assert_eq!(pull_request.base.commit, start);
    assert_eq!(pull_request.head.branch, thread.branch);
    assert_eq!(pull_request.head.commit, fixture.tip(&thread.branch));
}

#[test]
fn an_answer_without_changes_opens_no_pull_request() {
    let fixture = Fixture::new();
    let id = fixture.create("What is the answer?").id;
    let thread = fixture.run(&id, Command::Start);
    assert_eq!(thread.ending, Some(Ending::Answer), "{thread:#?}");
    assert_eq!(thread.pull_request, None);
    assert_eq!(thread.final_message(), Some("The answer is 42."));
    assert_eq!(fixture.tip(&thread.branch), thread.start.commit);
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
        git(&fixture.root, &["log", "-1", "--format=%B", &thread.branch]),
        format!(
            "{}\n\nChanges the agent left uncommitted, committed by GitEZ.",
            thread.title
        )
    );
    assert_eq!(
        git(
            &fixture.root,
            &["show", &format!("{}:left.txt", thread.branch)]
        ),
        "left"
    );
    assert_eq!(git(&thread.worktree, &["status", "--porcelain"]), "");
    assert_eq!(thread.pull_request, Some(1));
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
    assert!(!git(&thread.worktree, &["status", "--porcelain"]).is_empty());
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
            &["rev-list", "--count", &format!("main..{}", thread.branch)]
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
