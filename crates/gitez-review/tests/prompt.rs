use gitez_review::{
    Anchor, PromptContext, PromptOptions, PullRequest, Resolution, ResolvedBy, Side, State,
    Template, Timestamp, Tip,
};

fn time() -> Timestamp {
    "2026-10-24T13:00:00Z".parse().expect("a time")
}

fn pull_request() -> PullRequest {
    PullRequest {
        number: 3,
        title: "Anchor review comments to diff hunks".to_owned(),
        description: String::new(),
        state: State::Open,
        base: Tip {
            branch: "main".to_owned(),
            commit: "4b81e3d0".repeat(5),
        },
        head: Tip {
            branch: "feature/review-comments".to_owned(),
            commit: "a3f9c21e".repeat(5),
        },
        created: time(),
        updated: time(),
        viewed: Default::default(),
        prompt: PromptOptions::default(),
        threads: Vec::new(),
    }
}

fn anchor(path: &str, lines: (u32, u32), excerpt: &[&str]) -> Anchor {
    Anchor {
        commit: "a3f9c21e".repeat(5),
        path: path.to_owned(),
        side: Side::New,
        lines,
        excerpt: excerpt.iter().map(|line| (*line).to_owned()).collect(),
    }
}

fn context() -> PromptContext {
    PromptContext {
        repository: "gitez".to_owned(),
        commits_ahead: Some(3),
        test_command: Some("pnpm test".to_owned()),
    }
}

/// The three threads of the artboard, added out of order.
fn artboard() -> PullRequest {
    let mut pull_request = pull_request();
    pull_request.add_thread(
        anchor(
            "src/review/store.ts",
            (12, 15),
            &["const REVIEW_DIR = 'gitez-reviews';"],
        ),
        "Keep reviews under the git dir (.git/gitez/reviews) instead of the working tree, so \
         they never show up as untracked files or get committed by accident."
            .to_owned(),
        time(),
    );
    pull_request.add_thread(
        anchor(
            "src/review/anchor.ts",
            (33, 34),
            &[
                "    if (hunk.oldStart === line) return hunk;",
                "    if (line > hunk.newStart && line < end) return hunk;",
            ],
        ),
        "This compares against oldStart, but line is a new-side number, and the range check \
         skips the hunk's last line. Use one inclusive range check on the new side, and add a \
         test for a comment on a hunk's last line."
            .to_owned(),
        time(),
    );
    pull_request.add_thread(
        anchor(
            "src/review/anchor.ts",
            (45, 46),
            &[
                "      offset: c.line - hunk!.newStart,",
                "      context: hunk!.lines.slice(0, 3),",
            ],
        ),
        "After the guard on line 41 the hunk can't be undefined, so drop the non-null \
         assertions."
            .to_owned(),
        time(),
    );
    pull_request
}

#[test]
fn the_prompt_lists_the_threads_by_file_and_line() {
    let expected = "\
Repository: gitez
Branch: feature/review-comments (3 commits ahead of main)

Address the review comments below. Change only what each comment asks and leave unrelated code as it is. Make one commit per comment, with a message starting \"review:\". When done, run `pnpm test` and report the result.

1. src/review/anchor.ts, lines 33–34
   if (hunk.oldStart === line) return hunk;
   if (line > hunk.newStart && line < end) return hunk;
   → This compares against oldStart, but line is a new-side number, and the range check skips the hunk's last line. Use one inclusive range check on the new side, and add a test for a comment on a hunk's last line.

2. src/review/anchor.ts, lines 45–46
   offset: c.line - hunk!.newStart,
   context: hunk!.lines.slice(0, 3),
   → After the guard on line 41 the hunk can't be undefined, so drop the non-null assertions.

3. src/review/store.ts, lines 12–15
   const REVIEW_DIR = 'gitez-reviews';
   → Keep reviews under the git dir (.git/gitez/reviews) instead of the working tree, so they never show up as untracked files or get committed by accident.
";
    assert_eq!(artboard().prompt(&context()).as_deref(), Some(expected));
}

#[test]
fn only_open_threads_marked_for_the_prompt_are_included() {
    let mut pull_request = artboard();
    pull_request.set_in_prompt(1, false).expect("thread 1");
    pull_request
        .set_resolution(
            2,
            Some(Resolution {
                by: ResolvedBy::Auto,
                commit: "b0".repeat(20),
                at: time(),
            }),
        )
        .expect("thread 2");
    let included: Vec<u32> = pull_request
        .prompt_threads()
        .iter()
        .map(|thread| thread.id)
        .collect();
    assert_eq!(included, [3]);
    let prompt = pull_request.prompt(&context()).expect("a prompt");
    assert!(prompt.contains("\n1. src/review/anchor.ts, lines 45–46\n"));
    assert!(!prompt.contains("2. "));

    pull_request.set_in_prompt(3, false).expect("thread 3");
    assert_eq!(pull_request.prompt(&context()), None);
}

#[test]
fn options_change_the_instructions_and_quotes() {
    let mut pull_request = pull_request();
    let id = pull_request.add_thread(
        Anchor {
            side: Side::Old,
            ..anchor("src/old.rs", (7, 7), &["\tfn removed() {", "", "\t}"])
        },
        "Why was this removed?\nIt was the only caller.".to_owned(),
        time(),
    );
    pull_request
        .reply(id, "And it had tests.".to_owned(), time())
        .expect("reply");

    let unknown = PromptContext {
        repository: "gitez".to_owned(),
        commits_ahead: None,
        test_command: None,
    };
    let prompt = pull_request.prompt(&unknown).expect("a prompt");
    assert!(prompt.contains("Branch: feature/review-comments, reviewed against main\n"));
    assert!(prompt.contains("When done, run the project's tests and report the result."));
    // One line, on the base's side; a blank quoted line stays empty; a
    // comment's further lines and replies follow it.
    assert!(prompt.ends_with(
        "\
1. src/old.rs, removed line 7
   fn removed() {

   }
   → Why was this removed?
     It was the only caller.
   → And it had tests.
"
    ));

    pull_request.prompt = PromptOptions {
        template: Template::Explain,
        quote_code: false,
        ..PromptOptions::default()
    };
    let prompt = pull_request.prompt(&context()).expect("a prompt");
    assert!(prompt.contains(
        "\nAnswer the review comments below by explaining the code each one points at. Do not \
         change any files.\n"
    ));
    assert!(!prompt.contains("one commit per comment"));
    assert!(!prompt.contains("pnpm"));
    assert!(!prompt.contains("fn removed"));

    pull_request.prompt = PromptOptions {
        run_tests: false,
        ..PromptOptions::default()
    };
    let prompt = pull_request.prompt(&context()).expect("a prompt");
    assert!(prompt.contains("starting \"review:\".\n"));
    assert!(!prompt.contains("pnpm"));
}
