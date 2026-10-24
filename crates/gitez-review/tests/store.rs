use std::fs;

use gitez_review::{
    Anchor, Error, NewPullRequest, PullRequest, Resolution, ResolvedBy, RunHeader, Side, Store,
    Timestamp, Tip,
};
use tempfile::TempDir;

fn store() -> (TempDir, Store) {
    let dir = TempDir::new().expect("temp dir");
    let store = Store::new(dir.path());
    (dir, store)
}

fn time(text: &str) -> Timestamp {
    text.parse().expect("an RFC 3339 time")
}

fn new_pull_request(title: &str) -> NewPullRequest {
    NewPullRequest {
        title: title.to_owned(),
        description: "Why and what.".to_owned(),
        base: Tip {
            branch: "main".to_owned(),
            commit: "4b81e3d0".repeat(5),
        },
        head: Tip {
            branch: "feature/review-comments".to_owned(),
            commit: "a3f9c21e".repeat(5),
        },
    }
}

fn anchor(lines: (u32, u32)) -> Anchor {
    Anchor {
        commit: "a3f9c21e".repeat(5),
        path: "src/review/anchor.ts".to_owned(),
        side: Side::New,
        lines,
        excerpt: vec!["if (hunk.oldStart === line) return hunk;".to_owned()],
    }
}

fn json(store: &Store, number: u32) -> serde_json::Value {
    let path = store.dir().join(number.to_string()).join("review.json");
    serde_json::from_str(&fs::read_to_string(path).expect("read")).expect("json")
}

#[test]
fn an_empty_store_has_no_pull_requests() {
    let (_dir, store) = store();
    assert_eq!(store.numbers().expect("numbers"), Vec::<u32>::new());
    assert!(matches!(store.load(1), Err(Error::NotFound(1))));
}

#[test]
fn pull_requests_are_numbered_and_read_back() {
    let (dir, store) = store();
    assert_eq!(store.dir(), dir.path().join("gitez/reviews"));
    let at = time("2026-10-24T13:02:11Z");
    let first = store.create(new_pull_request("First"), at).expect("create");
    let second = store
        .create(new_pull_request("Second"), at)
        .expect("create");
    assert_eq!((first.number, second.number), (1, 2));
    assert_eq!(store.numbers().expect("numbers"), [1, 2]);
    assert_eq!(store.load(2).expect("load"), second);

    // The file is the documented format, readable by hand.
    let saved = json(&store, 1);
    assert_eq!(saved["version"], 1);
    assert_eq!(saved["number"], 1);
    assert_eq!(saved["state"], "open");
    assert_eq!(saved["base"]["branch"], "main");
    assert_eq!(saved["created"], "2026-10-24T13:02:11Z");
    assert_eq!(saved["prompt"]["agent"], "claude-code");
    assert_eq!(saved["prompt"]["template"], "fix");
    assert_eq!(saved["threads"], serde_json::json!([]));
}

#[test]
fn a_number_whose_folder_exists_is_skipped() {
    let (_dir, store) = store();
    // Left by a crash between claiming the number and saving.
    fs::create_dir_all(store.dir().join("1")).expect("mkdir");
    let created = store
        .create(new_pull_request("After"), time("2026-10-24T13:00:00Z"))
        .expect("create");
    assert_eq!(created.number, 2);
    assert_eq!(store.numbers().expect("numbers"), [2]);
}

#[test]
fn threads_are_saved_with_their_comments() {
    let (_dir, store) = store();
    let mut pull_request: PullRequest = store
        .create(new_pull_request("Threads"), time("2026-10-24T13:00:00Z"))
        .expect("create");
    let first = pull_request.add_thread(
        anchor((33, 34)),
        "Use one inclusive range check.".to_owned(),
        time("2026-10-24T13:04:55Z"),
    );
    let second = pull_request.add_thread(
        anchor((45, 46)),
        "Drop the assertions.".to_owned(),
        time("2026-10-24T13:09:00Z"),
    );
    let reply = pull_request
        .reply(
            first,
            "And add a test.".to_owned(),
            time("2026-10-24T13:10:00Z"),
        )
        .expect("reply");
    assert_eq!((first, second, reply), (1, 2, 3));
    pull_request
        .set_resolution(
            second,
            Some(Resolution {
                by: ResolvedBy::Auto,
                commit: "c0ffee00".repeat(5),
                at: time("2026-10-24T14:00:00Z"),
            }),
        )
        .expect("resolve");
    pull_request.set_in_prompt(first, false).expect("in prompt");
    assert!(matches!(
        pull_request.reply(9, String::new(), time("2026-10-24T14:00:00Z")),
        Err(Error::NoThread {
            number: 1,
            thread: 9
        })
    ));
    store.save(&pull_request).expect("save");

    assert_eq!(store.load(1).expect("load"), pull_request);
    let saved = json(&store, 1);
    let threads = &saved["threads"];
    assert_eq!(threads[0]["anchor"]["lines"], serde_json::json!([33, 34]));
    assert_eq!(threads[0]["anchor"]["side"], "new");
    assert_eq!(threads[0]["in_prompt"], false);
    assert_eq!(threads[0]["resolved"], serde_json::Value::Null);
    assert_eq!(threads[0]["comments"][1]["body"], "And add a test.");
    assert_eq!(threads[1]["resolved"]["by"], "auto");
    assert_eq!(saved["updated"], "2026-10-24T14:00:00Z");

    // Reopened.
    pull_request.set_resolution(second, None).expect("reopen");
    store.save(&pull_request).expect("save");
    let reloaded = store.load(1).expect("load");
    assert!(reloaded.thread(second).expect("thread").is_open());
}

#[test]
fn a_locked_pull_request_is_not_overwritten() {
    let (_dir, store) = store();
    let mut pull_request = store
        .create(new_pull_request("Locked"), time("2026-10-24T13:00:00Z"))
        .expect("create");
    let folder = store.dir().join("1");
    let lock = folder.join("review.json.lock");
    let before = fs::read_to_string(folder.join("review.json")).expect("read");

    fs::write(&lock, "").expect("lock");
    pull_request.title = "Changed".to_owned();
    assert!(matches!(
        store.save(&pull_request),
        Err(Error::Locked { .. })
    ));
    assert_eq!(
        fs::read_to_string(folder.join("review.json")).expect("read"),
        before
    );
    // Someone else's lock is left alone.
    assert!(lock.exists());

    fs::remove_file(&lock).expect("unlock");
    store.save(&pull_request).expect("save");
    assert_eq!(store.load(1).expect("load").title, "Changed");
    assert!(!lock.exists());
}

#[test]
fn a_newer_format_is_refused_and_kept() {
    let (_dir, store) = store();
    let pull_request = store
        .create(new_pull_request("Newer"), time("2026-10-24T13:00:00Z"))
        .expect("create");
    let path = store.dir().join("1/review.json");
    let newer = r#"{ "version": 2, "number": 1, "something": "new" }"#;
    fs::write(&path, newer).expect("write");

    assert!(matches!(
        store.load(1),
        Err(Error::TooNew { version: 2, .. })
    ));
    assert!(matches!(
        store.save(&pull_request),
        Err(Error::TooNew { version: 2, .. })
    ));
    assert_eq!(fs::read_to_string(&path).expect("read"), newer);
    assert!(!store.dir().join("1/review.json.lock").exists());
}

#[test]
fn agent_runs_are_kept_as_transcripts() {
    let (_dir, store) = store();
    store
        .create(new_pull_request("Runs"), time("2026-10-24T13:00:00Z"))
        .expect("create");
    let header = RunHeader {
        started: time("2026-10-24T13:20:05Z"),
        agent: "claude-code".to_owned(),
        prompt: "Address the review comments below.\n1. …".to_owned(),
    };
    let mut run = store.start_run(1, header.clone()).expect("start");
    run.append(r#"{"type":"system","subtype":"init"}"#)
        .expect("append");
    run.append("{\"type\":\"result\"}\n").expect("append");
    // A second run in the same second gets its own file.
    let again = store.start_run(1, header.clone()).expect("start");

    let runs = store.runs(1).expect("runs");
    let names: Vec<_> = runs
        .iter()
        .map(|path| {
            path.file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(
        names,
        ["2026-10-24T13-20-05Z.jsonl", "2026-10-24T13-20-05Z-2.jsonl"]
    );
    assert_eq!(runs[0], run.path());
    assert_eq!(runs[1], again.path());

    let (read_header, lines) = store.read_run(run.path()).expect("read");
    assert_eq!(read_header, header);
    assert_eq!(
        lines,
        [
            r#"{"type":"system","subtype":"init"}"#,
            r#"{"type":"result"}"#
        ]
    );
    let first_line = fs::read_to_string(run.path()).expect("read");
    let first_line: serde_json::Value =
        serde_json::from_str(first_line.lines().next().expect("a line")).expect("json");
    assert_eq!(first_line["gitez"], "run");
    assert_eq!(first_line["started"], "2026-10-24T13:20:05Z");
}

#[test]
fn moving_the_head_resolves_changed_threads_and_moves_the_rest() {
    use gitez_review::{FileUpdate, HeadMoved, LineChange};

    let (_dir, store) = store();
    let mut pull_request = store
        .create(new_pull_request("Follow"), time("2026-10-24T13:00:00Z"))
        .expect("create");
    let old_head = pull_request.head.commit.clone();
    let new_head = "b0b0b0b0".repeat(5);
    let at = time("2026-10-24T15:00:00Z");
    let on = |path: &str, lines: (u32, u32)| Anchor {
        path: path.to_owned(),
        ..anchor(lines)
    };
    let comment = |text: &str| text.to_owned();
    let t0 = time("2026-10-24T13:05:00Z");

    // In anchor.ts, lines 5–7 are rewritten and two lines are inserted
    // before line 3.
    let edited = pull_request.add_thread(on("src/anchor.ts", (5, 6)), comment("a"), t0);
    let below = pull_request.add_thread(on("src/anchor.ts", (20, 21)), comment("b"), t0);
    let resolved_below = pull_request.add_thread(on("src/anchor.ts", (30, 30)), comment("c"), t0);
    let resolved_edited = pull_request.add_thread(on("src/anchor.ts", (6, 6)), comment("d"), t0);
    for thread in [resolved_below, resolved_edited] {
        pull_request
            .set_resolution(
                thread,
                Some(Resolution {
                    by: ResolvedBy::You,
                    commit: old_head.clone(),
                    at: t0,
                }),
            )
            .expect("resolve");
    }
    // store.ts is renamed without touching the commented line; gone.ts is
    // deleted; same.ts does not change.
    let renamed = pull_request.add_thread(on("src/store.ts", (12, 15)), comment("e"), t0);
    let deleted = pull_request.add_thread(on("src/gone.ts", (1, 1)), comment("f"), t0);
    let same = pull_request.add_thread(on("src/same.ts", (3, 3)), comment("g"), t0);
    // Not followed: on the base's side, or made on an older commit.
    let old_side = pull_request.add_thread(
        Anchor {
            side: Side::Old,
            ..on("src/anchor.ts", (5, 5))
        },
        comment("h"),
        t0,
    );
    let older = pull_request.add_thread(
        Anchor {
            commit: "0ld0ld00".repeat(5),
            ..on("src/anchor.ts", (5, 5))
        },
        comment("i"),
        t0,
    );

    let mut asked = Vec::new();
    let moved = pull_request
        .move_head::<()>(&new_head, at, |path| {
            asked.push(path.to_owned());
            Ok(match path {
                "src/anchor.ts" => FileUpdate::Changed {
                    path: path.to_owned(),
                    lines: vec![
                        LineChange {
                            old: 2..2,
                            new: 2..4,
                        },
                        LineChange {
                            old: 4..7,
                            new: 6..10,
                        },
                    ],
                },
                "src/store.ts" => FileUpdate::Changed {
                    path: "src/storage.ts".to_owned(),
                    lines: vec![LineChange {
                        old: 0..1,
                        new: 0..1,
                    }],
                },
                "src/gone.ts" => FileUpdate::Gone,
                _ => FileUpdate::Unchanged,
            })
        })
        .expect("move");

    assert_eq!(
        moved,
        HeadMoved {
            resolved: vec![edited, deleted],
            followed: vec![below, resolved_below, renamed, same],
        }
    );
    // Each file is read once.
    assert_eq!(
        asked,
        [
            "src/anchor.ts",
            "src/store.ts",
            "src/gone.ts",
            "src/same.ts"
        ]
    );
    assert_eq!(pull_request.head.commit, new_head);
    assert_eq!(pull_request.updated, at);

    let thread = |id| pull_request.thread(id).expect("thread");
    let auto = Some(Resolution {
        by: ResolvedBy::Auto,
        commit: new_head.clone(),
        at,
    });
    assert_eq!(thread(edited).resolved, auto);
    assert_eq!(thread(deleted).resolved, auto);
    // Two lines inserted above, three replaced by four: three lines down.
    assert_eq!(thread(below).anchor.lines, (23, 24));
    assert_eq!(thread(below).anchor.commit, new_head);
    assert_eq!(thread(resolved_below).anchor.lines, (33, 33));
    assert_eq!(thread(renamed).anchor.path, "src/storage.ts");
    assert_eq!(thread(renamed).anchor.lines, (12, 15));
    assert_eq!(thread(same).anchor.commit, new_head);
    // Left alone.
    assert_eq!(thread(resolved_edited).anchor.commit, old_head);
    assert_eq!(
        thread(resolved_edited).resolved.as_ref().map(|r| r.by),
        Some(ResolvedBy::You)
    );
    assert_eq!(thread(old_side).anchor.commit, old_head);
    assert!(thread(old_side).is_open());
    assert!(thread(older).is_open());

    // Moving to the same commit again changes nothing and reads nothing.
    let again = pull_request
        .move_head::<()>(&new_head, at, |_| panic!("nothing to read"))
        .expect("move");
    assert_eq!(again, HeadMoved::default());
}

#[test]
fn a_failed_read_stops_the_move() {
    use gitez_review::FileUpdate;

    let (_dir, store) = store();
    let mut pull_request = store
        .create(new_pull_request("Error"), time("2026-10-24T13:00:00Z"))
        .expect("create");
    let t0 = time("2026-10-24T13:00:00Z");
    pull_request.add_thread(anchor((1, 1)), "x".to_owned(), t0);
    pull_request.add_thread(
        Anchor {
            path: "src/other.ts".to_owned(),
            ..anchor((1, 1))
        },
        "y".to_owned(),
        t0,
    );
    let before = pull_request.clone();
    // The first file reads fine, the second does not.
    let result = pull_request.move_head(
        "new",
        time("2026-10-24T15:00:00Z"),
        |path| -> Result<FileUpdate, &str> {
            match path {
                "src/other.ts" => Err("no such commit"),
                _ => Ok(FileUpdate::Gone),
            }
        },
    );
    assert_eq!(result, Err("no such commit"));
    assert_eq!(pull_request, before);
}
