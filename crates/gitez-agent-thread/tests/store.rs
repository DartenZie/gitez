//! The agent thread store, transcripts and the global index, in temporary
//! directories.

mod common;

use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

use common::{EXAMPLE, Fixture, at};
use gitez_agent_thread::{
    AgentThreadId, Error, Index, IndexedRepository, RunHeader, State, Store, Timestamp, TurnKind,
    repository_key,
};
use gitez_sandbox::NetworkProfile;

const EXAMPLE_ID: &str = "add-a-blame-view-to-the-commit-detail";

fn example_store() -> (tempfile::TempDir, Store, AgentThreadId) {
    let dir = tempfile::tempdir().expect("temporary directory");
    let store = Store::new(dir.path());
    let id = AgentThreadId::new(EXAMPLE_ID).expect("valid id");
    let thread_dir = store.thread_dir(&id);
    fs::create_dir_all(&thread_dir).expect("create folder");
    fs::write(thread_dir.join("thread.json"), EXAMPLE).expect("write");
    (dir, store, id)
}

/// The top-level keys of a pretty-printed JSON object, in file order.
fn top_level_keys(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| line.strip_prefix("  \""))
        .filter_map(|line| line.split_once('"'))
        .map(|(key, _)| key.to_owned())
        .collect()
}

#[test]
fn a_saved_thread_matches_the_documented_format() {
    let (_dir, store, id) = example_store();
    assert_eq!(store.ids().expect("ids"), std::slice::from_ref(&id));
    let thread = store.load(&id).expect("load");
    assert_eq!(thread.title, "Add a blame view to the commit detail");

    let before = Timestamp::now();
    store
        .update(&id, |thread| {
            thread
                .warnings
                .push("Moved while the agent ran: main.".to_owned());
            Ok(())
        })
        .expect("update");
    let text = fs::read_to_string(store.thread_dir(&id).join("thread.json")).expect("read");
    assert!(text.ends_with("}\n"), "a newline at the end");
    assert_eq!(
        top_level_keys(&text),
        top_level_keys(EXAMPLE),
        "version first, then the fields in the documented order"
    );

    let written: serde_json::Value = serde_json::from_str(&text).expect("parses");
    let mut expected: serde_json::Value = serde_json::from_str(EXAMPLE).expect("parses");
    let updated: Timestamp = written["updated"]
        .as_str()
        .expect("a time")
        .parse()
        .expect("RFC 3339");
    assert!(updated >= before, "updated is set to now");
    expected["updated"] = written["updated"].clone();
    expected["warnings"] = serde_json::json!(["Moved while the agent ran: main."]);
    assert_eq!(written, expected);
}

#[test]
fn an_update_that_changes_nothing_writes_nothing() {
    let (_dir, store, id) = example_store();
    let path = store.thread_dir(&id).join("thread.json");
    store.update(&id, |_| Ok(())).expect("update");
    assert_eq!(fs::read_to_string(&path).expect("read"), EXAMPLE);
    let result: Result<(), Error> = store.update(&id, |thread| {
        thread.title = "Changed".to_owned();
        Err(Error::Refused("No.".to_owned()))
    });
    assert!(matches!(result, Err(Error::Refused(_))));
    assert_eq!(fs::read_to_string(&path).expect("read"), EXAMPLE);
    assert!(!store.thread_dir(&id).join("thread.json.lock").exists());
}

#[test]
fn a_newer_version_is_refused_and_kept() {
    let (_dir, store, id) = example_store();
    let path = store.thread_dir(&id).join("thread.json");
    let newer = EXAMPLE.replace("\"version\": 1", "\"version\": 2");
    fs::write(&path, &newer).expect("write");
    assert!(matches!(
        store.load(&id),
        Err(Error::TooNew { version: 2, .. })
    ));
    assert!(matches!(
        store.update(&id, |thread| {
            thread.title = "Changed".to_owned();
            Ok(())
        }),
        Err(Error::TooNew { version: 2, .. })
    ));
    assert_eq!(fs::read_to_string(&path).expect("read"), newer);
    let (threads, errors) = store.load_all();
    assert!(threads.is_empty());
    assert!(matches!(errors.as_slice(), [(listed, Error::TooNew { .. })] if *listed == id));
}

#[test]
fn an_update_waits_for_a_held_lock() {
    let (_dir, store, id) = example_store();
    let lock = store.thread_dir(&id).join("thread.json.lock");
    fs::write(&lock, "").expect("hold the lock");
    let release = {
        let lock = lock.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            fs::remove_file(lock).expect("release the lock");
        })
    };
    let started = Instant::now();
    store
        .update(&id, |thread| {
            thread.title = "Changed".to_owned();
            Ok(())
        })
        .expect("update after the lock is released");
    assert!(started.elapsed() >= Duration::from_millis(150));
    release.join().expect("released");
    assert_eq!(store.load(&id).expect("load").title, "Changed");
}

#[test]
fn an_update_gives_up_on_a_lock_held_too_long() {
    let (_dir, store, id) = example_store();
    let lock = store.thread_dir(&id).join("thread.json.lock");
    fs::write(&lock, "").expect("hold the lock");
    let started = Instant::now();
    let result = store.update(&id, |thread| {
        thread.title = "Changed".to_owned();
        Ok(())
    });
    let waited = started.elapsed();
    assert!(
        matches!(&result, Err(Error::Locked { path }) if *path == lock),
        "{result:?}"
    );
    assert!(waited >= Duration::from_millis(4900), "waited {waited:?}");
    assert!(waited < Duration::from_secs(10), "waited {waited:?}");
    assert!(lock.exists(), "someone else's lock is left alone");
    assert_eq!(
        store.load(&id).expect("load").title,
        "Add a blame view to the commit detail"
    );
}

#[test]
fn concurrent_updates_lose_nothing() {
    let (_dir, store, id) = example_store();
    let writers: Vec<_> = ["a", "b"]
        .into_iter()
        .map(|name| {
            let store = store.clone();
            let id = id.clone();
            std::thread::spawn(move || {
                for n in 0..25 {
                    store
                        .update(&id, |thread| {
                            thread.warnings.push(format!("{name}{n}"));
                            Ok(())
                        })
                        .expect("update");
                }
            })
        })
        .collect();
    for writer in writers {
        writer.join().expect("writer finished");
    }
    let warnings = store.load(&id).expect("load").warnings;
    assert_eq!(warnings.len(), 50);
    for name in ["a", "b"] {
        let mine: Vec<&String> = warnings.iter().filter(|w| w.starts_with(name)).collect();
        let expected: Vec<String> = (0..25).map(|n| format!("{name}{n}")).collect();
        assert_eq!(mine, expected.iter().collect::<Vec<_>>(), "in order");
    }
}

#[test]
fn create_names_the_thread_after_the_prompt() {
    let fixture = Fixture::new();
    let store = fixture.store();
    assert!(
        store.ids().expect("ids").is_empty(),
        "a missing store is empty"
    );
    let created = at("2026-11-13T09:41:07Z");
    let thread = fixture.create("Fix the flaky tests. They fail on CI.");
    assert_eq!(thread.title, "Fix the flaky tests");
    assert_eq!(thread.id.as_str(), "fix-the-flaky-tests");
    assert_eq!(thread.branch, "agent/fix-the-flaky-tests");
    assert_eq!(thread.state, State::Queued);
    assert_eq!(thread.created, created);
    assert_eq!(thread.updated, created);
    assert_eq!(thread.model.as_deref(), Some("haiku"));
    assert_eq!(thread.secrets, ["GITHUB_TOKEN"]);
    assert!(!thread.prepared);
    assert_eq!(thread.turns.len(), 1);
    assert_eq!(thread.turns[0].kind, TurnKind::Start);
    assert_eq!(
        thread.turns[0].prompt,
        "Fix the flaky tests. They fail on CI."
    );
    assert!(!thread.turns[0].resume);
    assert_eq!(thread.pending_turn(), Some(0));
    assert_eq!(
        thread.worktree,
        fixture
            .data_dir
            .join("worktrees")
            .join(repository_key("my repo", &fixture.common_dir()))
            .join("fix-the-flaky-tests")
    );
    assert!(
        thread
            .worktree
            .to_str()
            .expect("utf-8")
            .contains("Application Support/GitEZ/worktrees/my-repo-"),
        "{}",
        thread.worktree.display()
    );
    assert_eq!(store.load(&thread.id).expect("load"), thread);

    // The same prompt again: the folder is taken.
    let second = fixture.create("Fix the flaky tests");
    assert_eq!(second.id.as_str(), "fix-the-flaky-tests-2");
    assert_eq!(second.branch, "agent/fix-the-flaky-tests-2");
}

#[test]
fn create_skips_ids_whose_branch_or_worktree_exists() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let thread = store
        .create(
            fixture.new_thread("Update the README"),
            &fixture.data_dir,
            "my repo",
            |branch| branch == "agent/update-the-readme",
            at("2026-11-13T09:41:07Z"),
        )
        .expect("create");
    assert_eq!(thread.id.as_str(), "update-the-readme-2");

    // A worktree folder left behind by a thread that is gone.
    let stale = thread.worktree.with_file_name("update-the-readme-3");
    fs::create_dir_all(&stale).expect("create folder");
    let next = store
        .create(
            fixture.new_thread("Update the README"),
            &fixture.data_dir,
            "my repo",
            |branch| branch == "agent/update-the-readme",
            at("2026-11-13T09:42:07Z"),
        )
        .expect("create");
    assert_eq!(next.id.as_str(), "update-the-readme-4");
}

#[test]
fn load_all_lists_the_newest_first_and_reports_broken_ones() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let mut ids = Vec::new();
    for (prompt, time) in [
        ("First thing", "2026-11-13T09:00:00Z"),
        ("Second thing", "2026-11-13T11:00:00Z"),
        ("Third thing", "2026-11-13T10:00:00Z"),
    ] {
        let thread = store
            .create(
                fixture.new_thread(prompt),
                &fixture.data_dir,
                "my repo",
                |_| false,
                at(time),
            )
            .expect("create");
        ids.push(thread.id);
    }
    let broken = AgentThreadId::new("broken").expect("valid");
    fs::create_dir_all(store.thread_dir(&broken)).expect("create folder");
    fs::write(store.thread_dir(&broken).join("thread.json"), "{").expect("write");
    // Being created: a folder without its file yet.
    fs::create_dir_all(store.dir().join("being-created")).expect("create folder");
    // Not a thread id.
    fs::create_dir_all(store.dir().join("Not An Id")).expect("create folder");

    let (threads, errors) = store.load_all();
    let order: Vec<&str> = threads.iter().map(|thread| thread.id.as_str()).collect();
    assert_eq!(order, ["second-thing", "third-thing", "first-thing"]);
    assert!(matches!(errors.as_slice(), [(id, Error::Invalid { .. })] if *id == broken));
}

fn header(started: &str) -> RunHeader {
    RunHeader {
        started: at(started),
        agent: "claude-code".to_owned(),
        prompt: "Add a blame view…".to_owned(),
        kind: TurnKind::Start,
        session_id: Some("6f1c2b8e".to_owned()),
        resume: false,
        model: Some("sonnet".to_owned()),
        network: NetworkProfile::Packages,
    }
}

#[test]
fn transcripts_are_written_sorted_and_followed() {
    let (_dir, store, id) = example_store();
    assert!(store.runs(&id).expect("runs").is_empty());

    let mut first = store
        .start_run(&id, header("2026-11-13T09:41:31Z"))
        .expect("start run");
    assert_eq!(first.file_name(), "2026-11-13T09-41-31Z.jsonl");
    let same_second = store
        .start_run(&id, header("2026-11-13T09:41:31Z"))
        .expect("start run");
    assert_eq!(same_second.file_name(), "2026-11-13T09-41-31Z-2.jsonl");
    let earlier = store
        .start_run(&id, header("2026-11-13T08:00:00Z"))
        .expect("start run");
    let names: Vec<String> = store
        .runs(&id)
        .expect("runs")
        .iter()
        .map(|path| {
            path.file_name()
                .expect("a name")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(
        names,
        [
            earlier.file_name(),
            first.file_name(),
            same_second.file_name()
        ]
    );

    let text = fs::read_to_string(first.path()).expect("read");
    assert_eq!(
        text,
        "{\"gitez\":\"run\",\"version\":1,\"started\":\"2026-11-13T09:41:31Z\",\"agent\":\"claude-code\",\
         \"prompt\":\"Add a blame view…\",\"kind\":\"start\",\"session_id\":\"6f1c2b8e\",\"resume\":false,\
         \"model\":\"sonnet\",\"network\":\"packages\"}\n"
    );

    // Following the transcript while it is written.
    let (lines, offset) = store.read_run_from(first.path(), 0).expect("read");
    assert!(lines.is_empty(), "the header is skipped");
    first.append(r#"{"type":"system"}"#).expect("append");
    first.append("{\"type\":\"assistant\"}\n").expect("append");
    let (lines, offset) = store.read_run_from(first.path(), offset).expect("read");
    assert_eq!(lines, [r#"{"type":"system"}"#, r#"{"type":"assistant"}"#]);
    // Half a line is not returned until it is whole.
    let mut raw = fs::OpenOptions::new()
        .append(true)
        .open(first.path())
        .expect("open");
    raw.write_all(br#"{"type":"res"#).expect("write");
    let (lines, same) = store.read_run_from(first.path(), offset).expect("read");
    assert!(lines.is_empty());
    assert_eq!(same, offset);
    raw.write_all(b"ult\"}\n").expect("write");
    let (lines, end) = store.read_run_from(first.path(), offset).expect("read");
    assert_eq!(lines, [r#"{"type":"result"}"#]);
    assert_eq!(end, fs::metadata(first.path()).expect("metadata").len());
    // From the start: everything after the header.
    let (lines, _) = store.read_run_from(first.path(), 0).expect("read");
    assert_eq!(lines.len(), 3);

    let (read_header, output) = store.read_run(first.path()).expect("read run");
    assert_eq!(read_header, header("2026-11-13T09:41:31Z"));
    assert_eq!(output, lines);

    let missing = AgentThreadId::new("missing").expect("valid");
    assert!(matches!(
        store.start_run(&missing, header("2026-11-13T09:41:31Z")),
        Err(Error::NotFound(_))
    ));
}

fn repository(common_dir: &Path, name: &str) -> IndexedRepository {
    IndexedRepository {
        common_dir: common_dir.to_owned(),
        work_dir: common_dir.parent().expect("a parent").to_owned(),
        name: name.to_owned(),
    }
}

#[test]
fn the_index_lists_each_repository_once() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let data_dir = dir.path().join("Application Support").join("GitEZ");
    let index = Index::new(&data_dir);
    assert_eq!(index.path(), data_dir.join("threads.json"));
    assert!(index.load().expect("load").is_empty(), "a missing file");

    let one = dir.path().join("one repo").join(".git");
    let two = dir.path().join("two").join(".git");
    fs::create_dir_all(&one).expect("create folder");
    fs::create_dir_all(&two).expect("create folder");
    index.add(repository(&one, "one repo")).expect("add");
    index.add(repository(&two, "two")).expect("add");
    index.add(repository(&one, "one repo")).expect("add again");
    // Another spelling of the same folder.
    index
        .add(repository(
            &dir.path()
                .join("one repo")
                .join("..")
                .join("one repo")
                .join(".git"),
            "one repo",
        ))
        .expect("add again");
    assert_eq!(
        index.load().expect("load"),
        [repository(&one, "one repo"), repository(&two, "two")]
    );
    assert!(!data_dir.join("threads.json.lock").exists());

    // A repository that is gone is left out, but stays in the file.
    fs::remove_dir_all(dir.path().join("two")).expect("remove");
    assert_eq!(index.load().expect("load"), [repository(&one, "one repo")]);
    let text = fs::read_to_string(index.path()).expect("read");
    assert!(text.contains("\"two\""));
    assert!(text.starts_with("{\n  \"version\": 1,\n  \"repositories\": ["));
}

#[test]
fn a_newer_index_is_read_as_empty_and_never_overwritten() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let index = Index::new(dir.path());
    let newer = "{\"version\": 2, \"repositories\": [], \"more\": true}\n";
    fs::write(index.path(), newer).expect("write");
    assert!(index.load().expect("load").is_empty());
    assert!(matches!(
        index.add(repository(dir.path(), "x")),
        Err(Error::TooNew { version: 2, .. })
    ));
    assert_eq!(fs::read_to_string(index.path()).expect("read"), newer);

    fs::write(index.path(), "not json").expect("write");
    assert!(matches!(index.load(), Err(Error::Invalid { .. })));
}
