//! The agent thread model: its JSON form and the changes its state allows.

mod common;

use common::{EXAMPLE, at, worktree_of};
use gitez_agent_thread::{
    AgentThread, AgentThreadId, BRIEF_PROMPT, CONTINUE_PROMPT, Ending, Error, HANDOFF_MARKER,
    Handoff, Mode, RetryPlan, State, Step, SupervisorInfo, TurnKind, TurnResult,
    network_changed_prompt, split_handoff, without_handoff_marker,
};
use gitez_sandbox::NetworkProfile;

fn example() -> AgentThread {
    serde_json::from_str(EXAMPLE).expect("the example parses")
}

/// The example, finished with a final message and a session.
fn finished(ending: Ending) -> AgentThread {
    let mut thread = example();
    thread.state = State::Finished;
    thread.ending = Some(ending);
    thread.supervisor = None;
    thread.activity = None;
    let turn = &mut thread.turns[0];
    turn.ended = Some(at("2026-11-13T10:01:31Z"));
    turn.result = Some(TurnResult {
        success: true,
        message: "Added the blame view.".to_owned(),
        cost_usd: Some(0.42),
        duration_ms: Some(1_200_000),
        steps: 40,
        exit_code: Some(0),
        stopped: false,
        blocked_hosts: Vec::new(),
    });
    thread
}

fn refusal(result: Result<impl std::fmt::Debug, Error>) -> String {
    match result {
        Err(Error::Refused(message)) => message,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn the_example_round_trips() {
    let thread = example();
    assert_eq!(thread.id.as_str(), "add-a-blame-view-to-the-commit-detail");
    assert_eq!(thread.network, NetworkProfile::Packages);
    assert_eq!(thread.state, State::Running);
    assert_eq!(
        thread.supervisor,
        Some(SupervisorInfo {
            pid: 48211,
            child_group: Some(48230),
            started: at("2026-11-13T09:41:08Z"),
        })
    );
    assert!(
        worktree_of(&thread)
            .to_str()
            .expect("utf-8")
            .contains("Application Support")
    );

    let mut value = serde_json::to_value(&thread).expect("serialises");
    value
        .as_object_mut()
        .expect("an object")
        .insert("version".to_owned(), 1.into());
    let expected: serde_json::Value = serde_json::from_str(EXAMPLE).expect("parses");
    assert_eq!(value, expected);
}

#[test]
fn empty_options_are_left_out_or_null_as_the_format_says() {
    let mut thread = example();
    thread.model = None;
    thread.session_id = None;
    thread.activity = None;
    thread.supervisor = None;
    let value = serde_json::to_value(&thread).expect("serialises");
    let object = value.as_object().expect("an object");
    for omitted in [
        "model",
        "session_id",
        "pull_request",
        "activity",
        "waiting_until",
        "error",
        "supervisor",
        "completed",
    ] {
        assert!(!object.contains_key(omitted), "{omitted} is omitted");
    }
    assert_eq!(object["ending"], serde_json::Value::Null);
    assert_eq!(object["turns"][0]["result"], serde_json::Value::Null);
    assert_eq!(object["warnings"], serde_json::json!([]));
}

#[test]
fn ids_are_slugs() {
    for valid in ["a", "fix-2", "add-a-blame-view", &"x".repeat(48)] {
        assert!(AgentThreadId::new(valid).is_ok(), "{valid}");
    }
    for invalid in [
        "",
        "-a",
        "a-",
        "a--b",
        "A",
        "a_b",
        "a/b",
        "é",
        &"x".repeat(49),
    ] {
        assert!(
            matches!(AgentThreadId::new(invalid), Err(Error::InvalidId(_))),
            "{invalid}"
        );
    }
    let id = AgentThreadId::new("fix-2").expect("valid");
    assert_eq!(id.branch(), "agent/fix-2");
    assert_eq!(id.to_string(), "fix-2");
    assert!(serde_json::from_str::<AgentThreadId>(r#""Not Valid""#).is_err());
}

#[test]
fn active_states_have_a_supervisor() {
    let active: Vec<State> = [
        State::Queued,
        State::Preparing,
        State::Running,
        State::Waiting,
        State::Finished,
        State::Completed,
    ]
    .into_iter()
    .filter(|state| state.is_active())
    .collect();
    assert_eq!(
        active,
        [
            State::Queued,
            State::Preparing,
            State::Running,
            State::Waiting
        ]
    );
}

#[test]
fn an_active_thread_refuses_turns_retries_and_completion() {
    let mut thread = example();
    let still = "The agent is still working on this thread.";
    let now = at("2026-11-13T10:00:00Z");
    assert_eq!(
        refusal(thread.request_turn(TurnKind::Reply, "More".to_owned(), now)),
        still
    );
    assert_eq!(
        refusal(thread.request_retry(NetworkProfile::Open, now)),
        still
    );
    assert_eq!(refusal(thread.mark_completed(now)), still);
    assert_eq!(thread, example(), "nothing changed");
}

#[test]
fn a_reply_queues_a_finished_thread_that_has_a_conversation() {
    let now = at("2026-11-13T10:05:00Z");
    let mut thread = finished(Ending::Answer);
    thread.session_id = None;
    assert_eq!(
        refusal(thread.request_turn(TurnKind::Reply, "More".to_owned(), now)),
        "Retry this thread first: the agent never started."
    );

    let mut thread = finished(Ending::PullRequest);
    thread.waiting_until = Some(now);
    assert!(
        thread
            .request_turn(TurnKind::Start, "Again".to_owned(), now)
            .is_err(),
        "only replies and reviews"
    );
    assert_eq!(thread.pending_turn(), None);
    thread
        .request_turn(TurnKind::Review, "Fix the comments".to_owned(), now)
        .expect("a review turn");
    assert_eq!(thread.state, State::Queued);
    assert_eq!(thread.ending, None);
    assert_eq!(thread.waiting_until, None);
    assert_eq!(thread.pending_turn(), Some(1));
    let turn = &thread.turns[1];
    assert_eq!(turn.kind, TurnKind::Review);
    assert_eq!(turn.prompt, "Fix the comments");
    assert!(turn.resume);
    assert_eq!(turn.requested, now);
    // The last message stays until the new turn has one.
    assert_eq!(thread.final_message(), Some("Added the blame view."));
}

#[test]
fn the_final_message_body_leaves_out_the_title_heading() {
    let mut thread = finished(Ending::PullRequest);
    let message = "# Add a blame view\n\nAdded the blame view.";
    thread.turns[0].result.as_mut().expect("result").message = message.to_owned();
    // The heading is not the title (yet): it is part of the message.
    assert_eq!(thread.final_message_body(), Some(message));
    thread.title = "Add a blame view".to_owned();
    assert_eq!(thread.final_message_body(), Some("Added the blame view."));
    assert_eq!(thread.final_message(), Some(message));
    // A heading alone leaves nothing to show.
    thread.turns[0].result.as_mut().expect("result").message = "# Add a blame view\n".to_owned();
    assert_eq!(thread.final_message_body(), None);
}

#[test]
fn only_a_failed_or_stopped_thread_can_be_retried() {
    let now = at("2026-11-13T10:05:00Z");
    for ending in [Ending::Answer, Ending::PullRequest] {
        let mut thread = finished(ending);
        assert_eq!(
            refusal(thread.request_retry(NetworkProfile::Open, now)),
            "Only a thread that failed or was stopped can be retried."
        );
    }
}

#[test]
fn the_retry_plan_follows_the_failed_step_and_the_conversation() {
    let now = at("2026-11-13T10:05:00Z");
    let cases = [
        (Step::Queue, false, RetryPlan::RunAgain),
        (Step::Worktree, false, RetryPlan::RunAgain),
        (Step::Prepare, false, RetryPlan::RunAgain),
        (Step::Sandbox, false, RetryPlan::RunAgain),
        (Step::Supervisor, false, RetryPlan::RunAgain),
        (Step::Agent, false, RetryPlan::RunAgain),
        (Step::Queue, true, RetryPlan::Continue),
        (Step::Supervisor, true, RetryPlan::Continue),
        (Step::Agent, true, RetryPlan::Continue),
        (Step::Commit, true, RetryPlan::Finish),
        (Step::PullRequest, false, RetryPlan::Finish),
    ];
    for (step, has_session, expected) in cases {
        let mut thread = finished(Ending::Answer);
        if !has_session {
            thread.session_id = None;
        }
        thread.fail(step, "It broke.", now);
        let plan = thread
            .request_retry(NetworkProfile::Open, now)
            .expect("a failed thread can be retried");
        assert_eq!(plan, expected, "{step:?}, session {has_session}");
        assert_eq!(thread.state, State::Queued);
        assert_eq!(thread.network, NetworkProfile::Open);
        assert_eq!(thread.ending, None);
        assert_eq!(thread.error, None);
        match plan {
            RetryPlan::Finish => {
                assert_eq!(thread.turns.len(), 1);
                assert_eq!(thread.pending_turn(), None);
            }
            RetryPlan::RunAgain => {
                let turn = &thread.turns[1];
                assert_eq!(turn.kind, TurnKind::Retry);
                assert_eq!(turn.prompt, thread.turns[0].prompt);
                assert!(!turn.resume);
                assert_eq!(thread.pending_turn(), Some(1));
            }
            RetryPlan::Continue => {
                let turn = &thread.turns[1];
                assert_eq!(turn.kind, TurnKind::Retry);
                assert_eq!(
                    turn.prompt,
                    network_changed_prompt(NetworkProfile::Packages, NetworkProfile::Open)
                );
                assert!(turn.resume);
            }
        }
    }

    for (has_session, expected) in [(true, RetryPlan::Continue), (false, RetryPlan::RunAgain)] {
        let mut thread = finished(Ending::Stopped);
        if !has_session {
            thread.session_id = None;
        }
        assert_eq!(
            thread
                .request_retry(NetworkProfile::AgentOnly, now)
                .expect("a stopped thread can be retried"),
            expected
        );
    }
}

#[test]
fn a_network_change_continues_a_finished_thread() {
    let now = at("2026-11-13T10:05:00Z");
    for ending in [Ending::PullRequest, Ending::Answer] {
        let mut thread = finished(ending);
        assert_eq!(thread.network, NetworkProfile::Packages);
        assert_eq!(
            thread
                .request_network_change(NetworkProfile::Open, now)
                .expect("a finished thread can change its network"),
            RetryPlan::Continue
        );
        assert_eq!(thread.state, State::Queued);
        assert_eq!(thread.ending, None);
        assert_eq!(thread.network, NetworkProfile::Open);
        let turn = thread.turns.last().expect("a turn");
        assert_eq!(turn.kind, TurnKind::Retry);
        assert!(turn.resume);
        assert_eq!(
            turn.prompt,
            network_changed_prompt(NetworkProfile::Packages, NetworkProfile::Open)
        );
        assert!(turn.prompt.contains("Try again what the network blocked"));
        assert_eq!(thread.pending_turn(), Some(1));
    }

    // Less network says nothing about what was blocked.
    let narrower = network_changed_prompt(NetworkProfile::Open, NetworkProfile::AgentOnly);
    assert!(
        narrower.contains("from Open to Agent API only"),
        "{narrower}"
    );
    assert!(!narrower.contains("blocked"), "{narrower}");

    // A failed or stopped thread is retried under the new network.
    let mut thread = finished(Ending::Answer);
    thread.fail(Step::Agent, "It broke.", now);
    assert_eq!(
        thread
            .request_network_change(NetworkProfile::Open, now)
            .expect("retry"),
        RetryPlan::Continue
    );
    assert_eq!(thread.network, NetworkProfile::Open);
    assert_eq!(
        thread.turns.last().expect("a turn").prompt,
        network_changed_prompt(NetworkProfile::Packages, NetworkProfile::Open)
    );

    // Refusals: the same network, a running or completed thread, and a
    // conversation that never started.
    let mut thread = finished(Ending::Answer);
    assert!(
        refusal(thread.request_network_change(NetworkProfile::Packages, now))
            .contains("already uses Packages")
    );
    let mut running = example();
    refusal(running.request_network_change(NetworkProfile::Open, now));
    thread.mark_completed(now).expect("complete");
    refusal(thread.request_network_change(NetworkProfile::Open, now));
    let mut thread = finished(Ending::Answer);
    thread.session_id = None;
    refusal(thread.request_network_change(NetworkProfile::Open, now));
    assert_eq!(thread.network, NetworkProfile::Packages);
    assert_eq!(thread.turns.len(), 1);
}

#[test]
fn a_retry_keeps_a_reply_that_never_ran() {
    let now = at("2026-11-13T10:05:00Z");
    let mut thread = finished(Ending::Answer);
    thread
        .request_turn(TurnKind::Reply, "Also add a test.".to_owned(), now)
        .expect("reply");
    thread.fail(Step::Sandbox, "The sandbox is not installed.", now);
    assert_eq!(
        thread
            .request_retry(NetworkProfile::Packages, now)
            .expect("retry"),
        RetryPlan::Continue
    );
    let turn = thread.turns.last().expect("a turn");
    assert_eq!(turn.kind, TurnKind::Retry);
    assert_eq!(turn.prompt, "Also add a test.");
    assert_eq!(thread.pending_turn(), Some(2));
}

#[test]
fn failing_ends_the_running_turn_and_clears_the_supervisor() {
    let mut thread = example();
    let now = at("2026-11-13T10:00:00Z");
    thread.fail(Step::Supervisor, "GitEZ's supervisor stopped.", now);
    assert_eq!(thread.state, State::Finished);
    assert_eq!(thread.ending, Some(Ending::Failed));
    let error = thread.error.as_ref().expect("an error");
    assert_eq!(error.step, Step::Supervisor);
    assert_eq!(error.message, "GitEZ's supervisor stopped.");
    assert_eq!(error.at, now);
    assert_eq!(thread.supervisor, None);
    assert_eq!(thread.activity, None);
    assert_eq!(thread.turns[0].ended, Some(now));
    // 09:41:31 to 10:00:00, and it no longer grows.
    let later = at("2026-11-13T12:00:00Z");
    assert_eq!(thread.run_time(later).as_secs(), 18 * 60 + 29);
}

#[test]
fn completing_is_final() {
    let now = at("2026-11-13T10:05:00Z");
    let mut thread = finished(Ending::Failed);
    thread.mark_completed(now).expect("complete");
    assert_eq!(thread.state, State::Completed);
    assert_eq!(thread.completed, Some(now));
    assert_eq!(thread.ending, Some(Ending::Failed));
    let completed = "This thread is completed.";
    assert_eq!(refusal(thread.mark_completed(now)), completed);
    assert_eq!(
        refusal(thread.request_turn(TurnKind::Reply, "More".to_owned(), now)),
        completed
    );
    assert_eq!(
        refusal(thread.request_retry(NetworkProfile::Open, now)),
        completed
    );
}

#[test]
fn steps_and_run_time_add_up_the_turns() {
    let thread = example();
    assert_eq!(thread.steps(), 31, "from the activity while it runs");
    // Running: counts up to now.
    assert_eq!(thread.run_time(at("2026-11-13T09:42:31Z")).as_secs(), 60);

    let mut thread = finished(Ending::Answer);
    assert_eq!(thread.steps(), 40, "from the results once finished");
    assert_eq!(
        thread.run_time(at("2026-11-14T00:00:00Z")).as_secs(),
        20 * 60
    );
    thread
        .request_turn(
            TurnKind::Reply,
            "More".to_owned(),
            at("2026-11-13T11:00:00Z"),
        )
        .expect("reply");
    let turn = &mut thread.turns[1];
    turn.started = Some(at("2026-11-13T11:00:00Z"));
    turn.ended = Some(at("2026-11-13T11:00:30Z"));
    turn.result = Some(TurnResult {
        success: true,
        message: String::new(),
        cost_usd: None,
        duration_ms: None,
        steps: 2,
        exit_code: Some(0),
        stopped: false,
        blocked_hosts: Vec::new(),
    });
    thread.state = State::Finished;
    assert_eq!(thread.steps(), 42);
    assert_eq!(
        thread.run_time(at("2026-11-14T00:00:00Z")).as_secs(),
        20 * 60 + 30
    );
    assert_eq!(
        thread.final_message(),
        Some("Added the blame view."),
        "an empty message does not hide the one before"
    );
    assert_eq!(thread.pending_turn(), None);
}

fn image(name: &str) -> gitez_agent_thread::TurnImage {
    gitez_agent_thread::TurnImage {
        name: name.to_owned(),
        media_type: gitez_agent_thread::ImageType::Png,
        size: 10,
        width: 4,
        height: 3,
    }
}

#[test]
fn a_retry_sends_the_images_of_the_prompt_it_sends_again() {
    let now = at("2026-11-13T10:05:00Z");

    // The agent never started: the first prompt again, with its images.
    let mut thread = finished(Ending::Failed);
    thread.session_id = None;
    thread.turns[0].images = vec![image("a-1.png"), image("a-2.png")];
    assert_eq!(
        thread
            .request_retry(NetworkProfile::AgentOnly, now)
            .expect("retry"),
        RetryPlan::RunAgain
    );
    let retry = thread.turns.last().expect("a turn");
    assert_eq!(retry.kind, TurnKind::Retry);
    assert_eq!(retry.images, thread.turns[0].images);

    // A reply with an image that never ran: sent again with it.
    let mut thread = finished(Ending::Answer);
    thread
        .request_turn_with_images(
            TurnKind::Reply,
            "What is this?".to_owned(),
            vec![image("b-1.png")],
            now,
        )
        .expect("reply");
    assert_eq!(thread.turns[1].images, [image("b-1.png")]);
    thread.fail(Step::Sandbox, "The sandbox is not installed.", now);
    thread
        .request_retry(NetworkProfile::AgentOnly, now)
        .expect("retry");
    let retry = thread.turns.last().expect("a turn");
    assert_eq!(retry.prompt, "What is this?");
    assert_eq!(retry.images, [image("b-1.png")]);

    // A turn the agent ran is continued; the images are in its
    // conversation already.
    let mut thread = finished(Ending::Stopped);
    thread.turns[0].images = vec![image("c-1.png")];
    thread
        .request_retry(NetworkProfile::Packages, now)
        .expect("retry");
    let retry = thread.turns.last().expect("a turn");
    assert_eq!(retry.prompt, CONTINUE_PROMPT);
    assert!(retry.images.is_empty());
}

#[test]
fn a_turn_without_images_has_no_images_field() {
    let thread = example();
    assert!(thread.turns.iter().all(|turn| turn.images.is_empty()));
    let text = serde_json::to_string(&thread).expect("json");
    assert!(!text.contains("\"images\""), "{text}");
    let mut with = thread.clone();
    with.turns[0].images = vec![image("2026-11-13T09-41-31Z-1.png")];
    let value = serde_json::to_value(&with).expect("json");
    assert_eq!(
        value["turns"][0]["images"],
        serde_json::json!([{
            "name": "2026-11-13T09-41-31Z-1.png",
            "media_type": "image/png",
            "size": 10,
            "width": 4,
            "height": 3,
        }])
    );
    let back: AgentThread = serde_json::from_value(value).expect("parses");
    assert_eq!(back, with);
}

/// A finished Discuss thread with a conversation.
fn discussion() -> AgentThread {
    let mut thread = finished(Ending::Answer);
    thread.mode = Mode::Discuss;
    thread.branch = None;
    thread.worktree = None;
    thread.work_dir = Some("/Users/me/repo".into());
    thread
}

#[test]
fn only_a_discuss_thread_writes_a_brief() {
    let now = at("2026-11-13T10:05:00Z");
    let mut implement = finished(Ending::Answer);
    assert_eq!(
        refusal(implement.request_turn(TurnKind::Brief, BRIEF_PROMPT.to_owned(), now)),
        "Only a Discuss thread can write a brief."
    );

    let mut thread = discussion();
    assert_eq!(
        refusal(thread.request_turn(TurnKind::Review, "Fix it".to_owned(), now)),
        "A Discuss thread has no pull request."
    );
    assert_eq!(thread.brief(), None, "an answer is not a brief");
    thread
        .request_turn(TurnKind::Brief, BRIEF_PROMPT.to_owned(), now)
        .expect("a brief turn");
    assert_eq!(thread.state, State::Queued);
    assert_eq!(thread.pending_turn(), Some(1));
    assert!(thread.turns[1].resume, "it continues the discussion");
    assert_eq!(thread.brief(), None, "not answered yet");

    thread.state = State::Finished;
    thread.ending = Some(Ending::Answer);
    let mut result = thread.turns[0].result.clone().expect("a result");
    result.message = "# Cache parsed config files\n\nGoal: …".to_owned();
    thread.turns[1].started = Some(now);
    thread.turns[1].ended = Some(now);
    thread.turns[1].result = Some(result.clone());
    assert_eq!(
        thread.brief(),
        Some("# Cache parsed config files\n\nGoal: …")
    );

    // A brief that failed, and a brief the discussion moved on from.
    result.success = false;
    thread.turns[1].result = Some(result);
    assert_eq!(thread.brief(), None);
    thread.turns[1].result.as_mut().expect("a result").success = true;
    thread
        .request_turn(TurnKind::Reply, "One more thing".to_owned(), now)
        .expect("a reply");
    assert_eq!(thread.brief(), None);
}

#[test]
fn the_brief_prompt_asks_for_a_title_and_a_brief_that_stands_alone() {
    assert!(BRIEF_PROMPT.contains("\"# <title>\""));
    assert!(BRIEF_PROMPT.contains("have not seen this conversation"));
    assert!(
        BRIEF_PROMPT.contains("its own.\n\nBegin with"),
        "paragraphs are kept apart"
    );
}

#[test]
fn a_mode_is_written_only_when_it_is_discuss() {
    assert_eq!(Mode::default(), Mode::Implement);
    let text = serde_json::to_string(&discussion()).expect("json");
    assert!(text.contains("\"mode\":\"discuss\""), "{text}");
    let text = serde_json::to_string(&finished(Ending::Answer)).expect("json");
    assert!(!text.contains("\"mode\""), "{text}");
    assert_eq!(Mode::ALL.map(Mode::label), ["Discuss", "Implement"]);
}

#[test]
fn the_handoff_marker_is_the_first_line_and_never_part_of_the_brief() {
    let marked = format!("{HANDOFF_MARKER}\n\n# Cache parsed config files\n\nGoal: …\n");
    assert_eq!(
        split_handoff(&marked),
        Some("# Cache parsed config files\n\nGoal: …")
    );
    // Blank lines, spaces and backticks around it are forgiven.
    assert_eq!(
        split_handoff(&format!("\n  `{HANDOFF_MARKER}`  \n# Title")),
        Some("# Title")
    );
    // Not marked: anywhere but the first line, or nothing after it.
    assert_eq!(split_handoff("# Title\n\nGoal: …"), None);
    assert_eq!(
        split_handoff(&format!("Sure.\n{HANDOFF_MARKER}\n# T")),
        None
    );
    assert_eq!(split_handoff(&format!("{HANDOFF_MARKER}\n\n")), None);

    assert_eq!(
        without_handoff_marker(&marked),
        "# Cache parsed config files\n\nGoal: …\n"
    );
    assert_eq!(without_handoff_marker(HANDOFF_MARKER), "");
    assert_eq!(without_handoff_marker("Just an answer."), "Just an answer.");
}

/// A finished Auto thread whose first turn the agent answered with a
/// brief.
fn handed_off() -> AgentThread {
    let mut thread = discussion();
    thread.auto = true;
    thread.turns[0].result.as_mut().expect("a result").message =
        "# Cache parsed config files\n\nGoal: …".to_owned();
    thread.turns[0].handoff = Some(Handoff::default());
    thread
}

#[test]
fn a_marked_turn_is_a_brief_claimed_at_most_once() {
    let now = at("2026-11-13T10:05:00Z");
    let mut thread = handed_off();
    assert_eq!(
        thread.brief(),
        Some("# Cache parsed config files\n\nGoal: …")
    );
    assert_eq!(thread.pending_handoff(), Some(0));

    assert_eq!(
        thread.claim_handoff(0, now).expect("claimed"),
        "# Cache parsed config files\n\nGoal: …"
    );
    assert_eq!(thread.pending_handoff(), None);
    assert_eq!(
        refusal(thread.claim_handoff(0, now)),
        "This brief was handed on already."
    );
    let id = AgentThreadId::new("cache-parsed-config-files").expect("an id");
    thread.record_handoff(0, Ok(id.clone()));
    assert_eq!(
        thread.turns[0].handoff,
        Some(Handoff {
            claimed: Some(now),
            thread: Some(id),
            problem: None,
        })
    );
    // The brief stays for "Implement…".
    assert!(thread.brief().is_some());

    // A claim survives a reload, as GitEZ reads the file again after a
    // restart.
    let text = serde_json::to_string(&thread).expect("json");
    let back: AgentThread = serde_json::from_str(&text).expect("parses");
    assert_eq!(back.pending_handoff(), None);

    // A later marked turn hands off once more.
    thread
        .request_turn(TurnKind::Reply, "And the other one".to_owned(), now)
        .expect("a reply");
    thread.state = State::Finished;
    let mut turn = thread.turns[0].clone();
    turn.kind = TurnKind::Reply;
    turn.handoff = Some(Handoff::default());
    thread.turns[1] = turn;
    assert_eq!(thread.pending_handoff(), Some(1));
}

#[test]
fn only_an_unclaimed_successful_brief_of_an_auto_discussion_is_pending() {
    let mut failed = handed_off();
    failed.turns[0].result.as_mut().expect("a result").success = false;
    assert_eq!(failed.pending_handoff(), None);

    let mut not_auto = handed_off();
    not_auto.auto = false;
    assert_eq!(not_auto.pending_handoff(), None);

    let mut completed = handed_off();
    completed.state = State::Completed;
    assert_eq!(completed.pending_handoff(), None);
}

#[test]
fn auto_is_written_only_when_set_and_older_records_read_as_before() {
    let text = serde_json::to_string(&discussion()).expect("json");
    assert!(!text.contains("\"auto\""), "{text}");
    assert!(!text.contains("\"handoff\""), "{text}");
    let back: AgentThread = serde_json::from_str(&text).expect("parses");
    assert!(!back.auto);
    assert_eq!(back.turns[0].handoff, None);

    let text = serde_json::to_string(&handed_off()).expect("json");
    assert!(text.contains("\"auto\":true"), "{text}");
    assert!(text.contains("\"handoff\":{}"), "{text}");
    let back: AgentThread = serde_json::from_str(&text).expect("parses");
    assert_eq!(back, handed_off());

    // The example is a record from before Auto.
    assert!(!example().auto);
    assert!(example().turns.iter().all(|turn| turn.handoff.is_none()));
}
