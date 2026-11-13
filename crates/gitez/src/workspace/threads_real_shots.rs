//! End-to-end runs of agent threads with the real Claude Code (haiku only)
//! in the real sandbox, for the paths the other real runs do not cover: an
//! answer completed by hand, Stop then Retry, a secret, the sandbox's file
//! rules, and two threads at once with and without a free slot. Each run
//! makes a throwaway repository under `GITEZ_E2E_SCRATCH`, with its own data
//! folder (srt copied from `GITEZ_E2E_SANDBOX`), and starts its threads with
//! the repository window's composer.
//!
//! They return at once unless `GITEZ_SCREENSHOTS` and `GITEZ_E2E_REAL` are
//! set; they also need `GITEZ_E2E_CLAUDE` (the real `claude`), optionally
//! `GITEZ_E2E_CLAUDE_CONFIG`, and a built `target/debug/gitez`. The secret
//! run keeps its value in the keychain under the service
//! `GITEZ_KEYCHAIN_SERVICE` (required, so the owner's own items are never
//! touched) and deletes it before it ends.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gitez_agent_thread::{AgentThread, Ending, State, Store};
use gitez_secret::SecretStore;
use gitez_settings::{AgentEntry, SecretEntry, SecretScope, Settings};
use gpui_kit::{AnyWindowHandle, Entity};

use crate::agent_threads::{AgentThreadKey, AgentThreads, OpenAgentPullRequest, OpenAgentThread};
use crate::screenshot::Shots;
use crate::workspace::Workspace;
use crate::workspace::composer_shots::{
    common_dir_of, gitez_binary, repository, supervisor_script, wait_until,
};

/// The model every run uses, as the owner allows.
const MODEL: &str = "haiku";

/// A repository window with real threads.
struct Real {
    shots: Shots,
    window: AnyWindowHandle,
    workspace: Entity<Workspace>,
    work_dir: PathBuf,
    common_dir: PathBuf,
    data_dir: PathBuf,
}

/// Whether the real runs are asked for.
fn wanted() -> bool {
    crate::screenshot::enabled() && std::env::var_os("GITEZ_E2E_REAL").is_some()
}

/// A throwaway repository `name` under the scratch folder, settings with
/// the real Claude Code (haiku by default), `limit` agents at once and
/// `secrets`, and its window.
fn real(
    name: &str,
    limit: u32,
    secrets: impl FnOnce(&Path) -> Vec<SecretEntry>,
    store: Option<Arc<dyn SecretStore>>,
) -> Real {
    real_with(name, limit, secrets, store, |_| {})
}

/// [`real`], with `prepare` run on the repository's working directory
/// before its window opens.
fn real_with(
    name: &str,
    limit: u32,
    secrets: impl FnOnce(&Path) -> Vec<SecretEntry>,
    store: Option<Arc<dyn SecretStore>>,
    prepare: impl FnOnce(&Path),
) -> Real {
    let binary = gitez_binary().expect("build target/debug/gitez first: cargo build -p gitez");
    let scratch = PathBuf::from(std::env::var_os("GITEZ_E2E_SCRATCH").expect("a scratch folder"));
    let sandbox = PathBuf::from(std::env::var_os("GITEZ_E2E_SANDBOX").expect("an srt install"));
    let base = scratch.join(format!("{name}-{}", std::process::id()));
    std::fs::create_dir_all(&base).expect("scratch folder");
    let base = base.canonicalize().expect("canonical");
    let work_dir = repository(&base);
    prepare(&work_dir);
    let common_dir = common_dir_of(&work_dir);
    let data_dir = base.join("data");
    std::fs::create_dir_all(&data_dir).expect("data folder");
    let copied = std::process::Command::new("cp")
        .arg("-R")
        .arg(&sandbox)
        .arg(data_dir.join("sandbox"))
        .status()
        .expect("cp runs");
    assert!(copied.success(), "copy srt");
    let claude =
        PathBuf::from(std::env::var_os("GITEZ_E2E_CLAUDE").expect("the path of the real claude"));
    let mut settings = Settings {
        agents: BTreeMap::from([(
            gitez_agent::ClaudeCode::ID.to_owned(),
            AgentEntry {
                program: Some(claude),
                config_dir: std::env::var_os("GITEZ_E2E_CLAUDE_CONFIG").map(PathBuf::from),
                model: Some(MODEL.to_owned()),
                ..AgentEntry::default()
            },
        )]),
        secrets: secrets(&common_dir),
        ..Settings::default()
    };
    settings.threads.max_running = limit;
    settings
        .save(&Settings::path(&data_dir))
        .expect("settings saved");
    let script = supervisor_script(&base, &binary, &data_dir, false);
    let mut shots = Shots::with_options(&data_dir, |options| {
        options.sandbox = gitez_sandbox::platform(&data_dir);
        options.exe = Ok(script);
        options.watch = true;
        if let Some(store) = store {
            options.secrets = store;
        }
    })
    .expect("enabled");
    let (window, workspace) = shots.open_workspace(&work_dir);
    Real {
        shots,
        window,
        workspace,
        work_dir,
        common_dir,
        data_dir,
    }
}

impl Real {
    fn threads(&mut self) -> Vec<Arc<AgentThread>> {
        let common_dir = self.common_dir.clone();
        self.shots.update(|cx| {
            AgentThreads::global(cx)
                .read(cx)
                .repository(&common_dir)
                .map(|repository| repository.threads.clone())
                .unwrap_or_default()
        })
    }

    /// Starts a thread on `prompt` with the composer under "Uncommitted
    /// changes", with haiku picked, and returns it once it is created.
    fn ask(&mut self, prompt: &str) -> AgentThreadKey {
        self.ask_with_images(prompt, Vec::new())
    }

    /// [`Real::ask`], with the PNGs `images` pasted into the prompt.
    fn ask_with_images(&mut self, prompt: &str, images: Vec<Vec<u8>>) -> AgentThreadKey {
        let before: Vec<_> = self
            .threads()
            .iter()
            .map(|thread| thread.id.clone())
            .collect();
        self.shots.keys(self.window, "cmd-shift-a");
        self.shots.type_text(self.window, prompt);
        let count = images.len();
        for image in images {
            self.shots.paste_image(self.window, image);
        }
        self.shots.settle();
        let workspace = self.workspace.clone();
        let attached = self.shots.update(|cx| {
            workspace
                .read(cx)
                .history
                .read(cx)
                .composer()
                .map(|composer| {
                    let images = composer.read(cx).images().read(cx);
                    (images.len(), images.preparing())
                })
        });
        assert_eq!(attached, Some((count, false)), "the images are ready");
        if count > 0 {
            self.save("final-image-0-composer");
        }
        let model = self.shots.update(|cx| {
            workspace
                .read(cx)
                .history
                .read(cx)
                .composer()
                .map(|composer| composer.read(cx).model_alias(cx))
        });
        assert_eq!(model.as_deref(), Some(MODEL), "haiku is picked");
        self.shots.keys(self.window, "cmd-enter");
        let threads = wait_until(
            &mut self.shots,
            &self.common_dir.clone(),
            Duration::from_secs(60),
            "the thread to be created",
            |threads| threads.iter().any(|thread| !before.contains(&thread.id)),
        );
        let thread = threads
            .iter()
            .find(|thread| !before.contains(&thread.id))
            .expect("the new thread");
        assert_eq!(thread.model.as_deref(), Some(MODEL), "{thread:#?}");
        AgentThreadKey {
            common_dir: self.common_dir.clone(),
            id: thread.id.clone(),
        }
    }

    fn wait(
        &mut self,
        key: &AgentThreadKey,
        timeout: Duration,
        what: &str,
        done: impl Fn(&AgentThread) -> bool,
    ) -> Arc<AgentThread> {
        let threads = wait_until(&mut self.shots, &key.common_dir, timeout, what, |threads| {
            threads
                .iter()
                .any(|thread| thread.id == key.id && done(thread))
        });
        threads
            .into_iter()
            .find(|thread| thread.id == key.id)
            .expect("the thread")
    }

    fn finished(&mut self, key: &AgentThreadKey, what: &str) -> Arc<AgentThread> {
        self.wait(key, Duration::from_secs(300), what, |thread| {
            !thread.state.is_active()
        })
    }

    fn open(&mut self, key: &AgentThreadKey) {
        self.shots.dispatch(
            self.window,
            OpenAgentThread {
                common_dir: key.common_dir.clone(),
                id: key.id.clone(),
            },
        );
    }

    fn save(&mut self, name: &str) {
        self.shots.save(self.window, name);
    }
}

/// What a thread said at the end of its last turn.
fn last_message(thread: &AgentThread) -> String {
    thread
        .turns
        .iter()
        .rev()
        .find_map(|turn| turn.result.as_ref())
        .map(|result| result.message.clone())
        .unwrap_or_default()
}

fn report(what: &str, started: Instant, thread: &AgentThread) {
    eprintln!(
        "{what}: {:?} after {:.1?}: {:?} {:?}, model {:?}, turns {}, error {:?}, \
         warnings {:?}\n  message: {}",
        thread.id,
        started.elapsed(),
        thread.state,
        thread.ending,
        thread.model,
        thread.turns.len(),
        thread.error,
        thread.warnings,
        last_message(thread)
    );
}

/// Every file under `dir` whose contents include `needle`.
fn files_containing(dir: &Path, needle: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut folders = vec![dir.to_owned()];
    while let Some(folder) = folders.pop() {
        let Ok(entries) = std::fs::read_dir(&folder) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                // srt's own install has nothing of the thread's.
                if path.file_name().is_some_and(|name| name == "node_modules") {
                    continue;
                }
                folders.push(path);
            } else if kind.is_file()
                && std::fs::read(&path).is_ok_and(|bytes| {
                    bytes
                        .windows(needle.len())
                        .any(|window| window == needle.as_bytes())
                })
            {
                found.push(path);
            }
        }
    }
    found
}

/// Path 3: a question changes nothing, so the thread ends as an answer
/// with no pull request; Complete removes its worktree and its branch.
#[test]
fn e2e_final_answer_then_complete() {
    if !wanted() {
        return;
    }
    let mut real = real("final-answer", 3, |_| Vec::new(), None);
    let started = Instant::now();
    let key = real.ask(
        "What does the function in parser.rs return? Answer in one short sentence. \
         Do not change, create or commit any file.",
    );
    let done = real.finished(&key, "the answer");
    report("answer", started, &done);
    assert_eq!(done.ending, Some(Ending::Answer), "{done:#?}");
    assert!(done.pull_request.is_none());
    real.open(&key);
    real.save("final-3-1-answer");
    real.shots.click_id(real.window, "thread-complete");
    let completed = real.wait(&key, Duration::from_secs(30), "completed", |thread| {
        thread.state == State::Completed
    });
    report("completed", started, &completed);
    assert!(!completed.worktree.exists(), "the worktree is gone");
    let branch = std::process::Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", &completed.branch])
        .current_dir(&real.work_dir)
        .status()
        .expect("git runs");
    assert!(!branch.success(), "a branch without commits is removed");
    real.save("final-3-2-completed");
}

/// Path 4: Stop ends a running agent's turn; Retry continues the same
/// thread, which then finishes.
#[test]
fn e2e_final_stop_then_retry() {
    if !wanted() {
        return;
    }
    let mut real = real("final-stop", 3, |_| Vec::new(), None);
    let started = Instant::now();
    let key = real.ask(
        "Run the shell command `sleep 40`, then reply with only the word done. \
         Do not change, create or commit any file.",
    );
    // Running, and the agent has used a tool (the sleep).
    let running = real.wait(
        &key,
        Duration::from_secs(120),
        "the agent to use a tool",
        |thread| {
            thread.state == State::Running
                && thread
                    .activity
                    .as_ref()
                    .is_some_and(|activity| activity.steps > 0)
        },
    );
    eprintln!(
        "running after {:.1?}: {:?}",
        started.elapsed(),
        running.activity
    );
    real.open(&key);
    real.save("final-4-1-running");
    let stopping = Instant::now();
    real.shots.click_id(real.window, "thread-stop");
    let stopped = real.wait(&key, Duration::from_secs(60), "the stop", |thread| {
        !thread.state.is_active()
    });
    report("stopped", started, &stopped);
    eprintln!("stopped {:.1?} after Stop", stopping.elapsed());
    assert_eq!(stopped.ending, Some(Ending::Stopped), "{stopped:#?}");
    assert!(stopped.worktree.exists(), "kept for a retry");
    real.save("final-4-2-stopped");
    let retrying = Instant::now();
    real.shots.click_id(real.window, "thread-retry");
    real.wait(
        &key,
        Duration::from_secs(60),
        "the retry to run",
        |thread| thread.turns.len() == 2,
    );
    let retried = real.wait(&key, Duration::from_secs(300), "the retry", |thread| {
        thread.turns.len() == 2 && !thread.state.is_active()
    });
    report("retried", retrying, &retried);
    assert_eq!(
        retried.session_id, stopped.session_id,
        "the same conversation"
    );
    // Claude Code's own telemetry, blocked in every run, does not make
    // Retry widen the network.
    assert_eq!(
        retried.network, stopped.network,
        "Retry keeps the network: {:?}",
        stopped.turns[0].result
    );
    assert!(
        retried.turns[1].resume,
        "the retry resumes: {:#?}",
        retried.turns
    );
    assert!(
        matches!(retried.ending, Some(Ending::Answer)),
        "{retried:#?}"
    );
    real.save("final-4-3-retried");
}

/// Path 6: a secret from the keychain reaches the agent's environment and
/// never GitEZ's files: the transcript and the thread show `[secret …]`.
#[test]
fn e2e_final_secret() {
    if !wanted() {
        return;
    }
    let service = std::env::var("GITEZ_KEYCHAIN_SERVICE")
        .ok()
        .filter(|service| !service.is_empty() && service != gitez_secret::DEFAULT_SERVICE)
        .expect("GITEZ_KEYCHAIN_SERVICE names a scratch keychain service");
    const NAME: &str = "GITEZ_FINAL_SECRET";
    let keychain = Arc::new(gitez_secret::Keychain::new(service));
    // Made up here, 32 characters; never printed.
    let value: String = {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        format!("fs{:030x}", seed ^ 0x5eed_c0ffee_u128)
            .chars()
            .take(32)
            .collect()
    };
    let entry = SecretEntry {
        name: NAME.into(),
        scope: SecretScope::All,
        default: true,
    };
    let account = entry.account();
    keychain.set(&account, &value).expect("the secret is saved");
    struct Remove(Arc<gitez_secret::Keychain>, String);
    impl Drop for Remove {
        fn drop(&mut self) {
            self.0.delete(&self.1).expect("the secret is deleted");
        }
    }
    let _remove = Remove(keychain.clone(), account);
    let mut real = real("final-secret", 3, |_| vec![entry], Some(keychain));
    let started = Instant::now();
    let key = real.ask(&format!(
        "Run the shell command `printf %s \"${NAME}\" | wc -c` and tell me the number it \
         prints. Then run `echo \"${NAME}\"` and quote its output in your reply. \
         Do not change, create or commit any file."
    ));
    let done = real.finished(&key, "the secret run");
    report("secret", started, &done);
    assert_eq!(done.secrets, [NAME]);
    assert!(
        last_message(&done).contains(&value.len().to_string()),
        "the agent saw a {}-character value",
        value.len()
    );
    // Nowhere in GitEZ's files: thread.json, transcripts, logs, the index.
    let leaks: Vec<PathBuf> = [real.common_dir.join("gitez"), real.data_dir.clone()]
        .iter()
        .flat_map(|dir| files_containing(dir, &value))
        .collect();
    assert_eq!(leaks, Vec::<PathBuf>::new(), "the value leaked");
    let store = Store::new(&real.common_dir);
    let marker = format!("[secret {NAME}]");
    let marked = files_containing(&store.thread_dir(&key.id).join("runs"), &marker);
    eprintln!("transcripts with {marker}: {marked:?}");
    assert!(!marked.is_empty(), "the transcript shows where it was");
    real.open(&key);
    real.save("final-6-1-secret");
}

/// Path 7: the sandbox holds with the real agent: no file in the home
/// folder or the main worktree, no listing of `~/.ssh`; the thread still
/// finishes with an answer.
#[test]
fn e2e_final_sandbox_holds() {
    if !wanted() {
        return;
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).expect("HOME");
    let mut real = real("final-sandbox", 3, |_| Vec::new(), None);
    let probe = format!("gitez-final-probe-{}.txt", std::process::id());
    let in_home = home.join(&probe);
    let in_main = real.work_dir.join(&probe);
    assert!(!in_home.exists() && !in_main.exists());
    let started = Instant::now();
    let key = real.ask(&format!(
        "This is a sandbox test. Run exactly these three shell commands, one at a time, \
         and for each say whether it worked or quote its error:\n\
         1. `touch {}`\n2. `touch {}`\n3. `ls {}`\n\
         Do not try any other way, and do not change, create or commit any other file.",
        in_home.display(),
        in_main.display(),
        home.join(".ssh").display(),
    ));
    let done = real.finished(&key, "the sandbox run");
    report("sandbox", started, &done);
    let home_written = in_home.exists();
    let main_written = in_main.exists();
    // Never leave a probe behind, whatever happened.
    std::fs::remove_file(&in_home).ok();
    std::fs::remove_file(&in_main).ok();
    assert!(!home_written, "the agent wrote in the home folder");
    assert!(!main_written, "the agent wrote in the main worktree");
    let transcripts = files_containing(
        &Store::new(&real.common_dir)
            .thread_dir(&key.id)
            .join("runs"),
        "Operation not permitted",
    );
    eprintln!("transcripts with refusals: {transcripts:?}");
    assert!(
        !transcripts.is_empty(),
        "the refusals are in the transcript"
    );
    assert_eq!(done.state, State::Finished);
    assert_eq!(done.ending, Some(Ending::Answer), "{done:#?}");
    real.open(&key);
    real.save("final-7-1-sandbox");
}

/// Starts two sleeping threads in one repository and waits for both.
fn two_threads(real: &mut Real) -> (AgentThreadKey, AgentThreadKey) {
    let prompt = |word: &str| {
        format!(
            "Run the shell command `sleep 15`, then reply with only the word {word}. \
             Do not change, create or commit any file."
        )
    };
    let first = real.ask(&prompt("one"));
    let second = real.ask(&prompt("two"));
    (first, second)
}

/// Path 8, a free slot each: two threads in one repository run at once.
#[test]
fn e2e_final_two_threads_at_once() {
    if !wanted() {
        return;
    }
    let mut real = real("final-two", 3, |_| Vec::new(), None);
    let started = Instant::now();
    let (first, second) = two_threads(&mut real);
    let ids = [first.id.clone(), second.id.clone()];
    wait_until(
        &mut real.shots,
        &real.common_dir.clone(),
        Duration::from_secs(120),
        "both to run at once",
        |threads| {
            threads
                .iter()
                .filter(|thread| ids.contains(&thread.id) && thread.state == State::Running)
                .count()
                == 2
        },
    );
    eprintln!("both running after {:.1?}", started.elapsed());
    real.save("final-8-1-two-running");
    let one = real.finished(&first, "the first");
    let two = real.finished(&second, "the second");
    report("first", started, &one);
    report("second", started, &two);
    assert_eq!(one.ending, Some(Ending::Answer), "{one:#?}");
    assert_eq!(two.ending, Some(Ending::Answer), "{two:#?}");
    assert_ne!(one.worktree, two.worktree);
    real.save("final-8-2-two-finished");
}

/// Path 8, one slot: the second thread waits as Queued and starts when
/// the first finishes.
#[test]
fn e2e_final_limit_one_queues() {
    if !wanted() {
        return;
    }
    let mut real = real("final-queue", 1, |_| Vec::new(), None);
    let started = Instant::now();
    let (first, second) = two_threads(&mut real);
    let (first_id, second_id) = (first.id.clone(), second.id.clone());
    wait_until(
        &mut real.shots,
        &real.common_dir.clone(),
        Duration::from_secs(120),
        "the first to run and the second to wait",
        |threads| {
            let state = |id| {
                threads
                    .iter()
                    .find(|thread| &thread.id == id)
                    .map(|thread| thread.state)
            };
            state(&first_id) == Some(State::Running) && state(&second_id) == Some(State::Queued)
        },
    );
    eprintln!("one running, one queued after {:.1?}", started.elapsed());
    real.save("final-8-3-queued");
    real.open(&second);
    real.save("final-8-4-queued-thread");
    let one = real.finished(&first, "the first");
    let two = real.finished(&second, "the second");
    report("first", started, &one);
    report("second", started, &two);
    let first_ended = one.turns[0].ended.expect("the first ended");
    let second_started = two.turns[0].started.expect("the second started");
    eprintln!("first ended {first_ended}, second started {second_started}");
    assert!(
        second_started >= first_ended,
        "the second waited for the first"
    );
    assert_eq!(one.ending, Some(Ending::Answer), "{one:#?}");
    assert_eq!(two.ending, Some(Ending::Answer), "{two:#?}");
    real.save("final-8-5-both-finished");
}

/// Path 1's last step for [`super::composer_shots`]: the pull request a
/// thread made opens in the window.
pub(crate) fn open_pull_request(
    shots: &mut Shots,
    window: AnyWindowHandle,
    workspace: &Entity<Workspace>,
    common_dir: &Path,
    number: u32,
) {
    shots.dispatch(
        window,
        OpenAgentPullRequest {
            common_dir: common_dir.to_owned(),
            number,
        },
    );
    let shown = shots.update(|cx| {
        workspace
            .read(cx)
            .review
            .as_ref()
            .and_then(|(review, _)| review.read(cx).pull_request_number())
    });
    assert_eq!(shown, Some(number), "the pull request opens");
}

/// The agent names the pull request: vague prompts, as people write them,
/// each changing a file. Reports the titles the agent gave (or did not
/// give) and what GitEZ made of them; asserts only that the pull request
/// and the thread share the title, since the agent may ignore the rule.
#[test]
fn e2e_final_agent_names_the_pull_request() {
    if !wanted() {
        return;
    }
    let mut real = real_with(
        "final-title",
        3,
        |_| Vec::new(),
        None,
        |work_dir| {
            std::fs::write(
                work_dir.join("greet.sh"),
                "#!/bin/sh\necho \"Hello.\"\necho \"Goodbye.\"\n",
            )
            .expect("write");
            for args in [
                &["add", "greet.sh"][..],
                &["commit", "--quiet", "-m", "Add the greeting script"],
            ] {
                let status = std::process::Command::new("git")
                    .args(args)
                    .current_dir(work_dir)
                    .status()
                    .expect("git runs");
                assert!(status.success(), "git {args:?}");
            }
        },
    );
    let started = Instant::now();
    let prompts = [
        "Can you look into the greeting and make it friendlier?",
        "Please change the goodbye in greet.sh, it sounds too cold",
        "look into this: greet.sh should greet the user by name when one is given",
    ];
    let keys: Vec<AgentThreadKey> = prompts.iter().map(|prompt| real.ask(prompt)).collect();
    let mut problems = Vec::new();
    for (index, (key, prompt)) in keys.iter().zip(prompts).enumerate() {
        let done = real.finished(key, "the change");
        report("title", started, &done);
        let message = last_message(&done);
        let heading = gitez_agent_thread::title_from_message(&message);
        eprintln!(
            "TITLE prompt {prompt:?}\n  first line: {:?}\n  heading: {heading:?}\n  \
             thread title: {:?}\n  pull request: {:?}",
            message.lines().find(|line| !line.trim().is_empty()),
            done.title,
            done.pull_request,
        );
        let Some(number) = done.pull_request else {
            problems.push(format!("{prompt:?} opened no pull request: {done:#?}"));
            continue;
        };
        let pull_request = gitez_review::Store::new(&real.common_dir)
            .load(number)
            .expect("load the pull request");
        eprintln!(
            "  pull request title: {:?}\n  description starts: {:?}",
            pull_request.title,
            pull_request.description.lines().next()
        );
        if pull_request.title != done.title {
            problems.push(format!(
                "{prompt:?}: pull request {:?} but thread {:?}",
                pull_request.title, done.title
            ));
        }
        real.open(key);
        real.save(&format!("final-title-{}-thread", index + 1));
        open_pull_request(
            &mut real.shots,
            real.window,
            &real.workspace.clone(),
            &real.common_dir.clone(),
            number,
        );
        real.save(&format!("final-title-{}-pull-request", index + 1));
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

/// Digits of a 5 × 7 bitmap font, a row of five bits a line.
const DIGITS: [[u8; 7]; 10] = [
    [0x0e, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0e],
    [0x04, 0x0c, 0x04, 0x04, 0x04, 0x04, 0x0e],
    [0x0e, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1f],
    [0x1f, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0e],
    [0x02, 0x06, 0x0a, 0x12, 0x1f, 0x02, 0x02],
    [0x1f, 0x10, 0x1e, 0x01, 0x01, 0x11, 0x0e],
    [0x06, 0x08, 0x10, 0x1e, 0x11, 0x11, 0x0e],
    [0x1f, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
    [0x0e, 0x11, 0x11, 0x0e, 0x11, 0x11, 0x0e],
    [0x0e, 0x11, 0x11, 0x0f, 0x01, 0x02, 0x0c],
];

/// A PNG of `number` drawn large, dark on light, that only a model which
/// sees the image can read.
fn number_image(number: u32) -> Vec<u8> {
    const SCALE: u32 = 24;
    let digits: Vec<usize> = number
        .to_string()
        .bytes()
        .map(|digit| usize::from(digit - b'0'))
        .collect();
    let margin = 2 * SCALE;
    let width = margin * 2 + digits.len() as u32 * 6 * SCALE;
    let height = margin * 2 + 7 * SCALE;
    let image = image::RgbImage::from_fn(width, height, |x, y| {
        let ink = (|| {
            let x = x.checked_sub(margin)? / SCALE;
            let y = y.checked_sub(margin)? / SCALE;
            let digit = digits.get((x / 6) as usize)?;
            let column = x % 6;
            let row = DIGITS[*digit].get(y as usize)?;
            Some(column < 5 && row & (0x10 >> column) != 0)
        })()
        .unwrap_or(false);
        if ink {
            image::Rgb([20, 20, 24])
        } else {
            image::Rgb([245, 243, 236])
        }
    });
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .expect("encode");
    bytes.into_inner()
}

/// A five-digit number nobody can guess: from the clock and the process.
fn unguessable_number(salt: u32) -> u32 {
    use std::hash::{BuildHasher as _, Hasher as _};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("after 1970")
            .as_nanos(),
    );
    hasher.write_u32(std::process::id());
    hasher.write_u32(salt);
    10_000 + (hasher.finish() % 90_000) as u32
}

/// Images: a pasted image reaches the model, on the first turn and in a
/// reply (`--resume`). Each shows a random number only a model that sees
/// it can read; neither the thread's transcripts nor GitEZ's data folder
/// keep the image's data, only the thread's `images` folder.
#[test]
fn e2e_final_image_prompt_and_reply() {
    if !wanted() {
        return;
    }
    let mut real = real("final-image", 3, |_| Vec::new(), None);
    let started = Instant::now();
    let first = unguessable_number(1);
    let second = unguessable_number(2);
    std::fs::write(real.data_dir.join("first.png"), number_image(first)).expect("write");
    std::fs::write(real.data_dir.join("second.png"), number_image(second)).expect("write");
    let key = real.ask_with_images(
        "What number is written in the attached image? Answer with its digits only. \
         Do not change, create or commit any file.",
        vec![number_image(first)],
    );
    let done = real.finished(&key, "the first answer");
    report("image", started, &done);
    let answer = last_message(&done);
    eprintln!("IMAGE first number {first}, answer {answer:?}");
    assert_eq!(done.ending, Some(Ending::Answer), "{done:#?}");
    assert_eq!(done.turns[0].images.len(), 1, "{done:#?}");
    real.open(&key);
    real.save("final-image-1-answer");
    // The reply box, at the bottom of the detail panel.
    real.shots.click(
        real.window,
        gpui_kit::point(gpui_kit::px(1200.), gpui_kit::px(781.)),
    );
    real.shots.type_text(
        real.window,
        "And the number in this image? Digits only, change nothing.",
    );
    real.shots.paste_image(real.window, number_image(second));
    real.save("final-image-2-reply");
    real.shots.keys(real.window, "cmd-enter");
    real.wait(&key, Duration::from_secs(60), "the reply", |thread| {
        thread.turns.len() == 2
    });
    let replied = real.wait(
        &key,
        Duration::from_secs(300),
        "the second answer",
        |thread| {
            thread.turns.len() == 2 && thread.turns[1].result.is_some() && !thread.state.is_active()
        },
    );
    report("image reply", started, &replied);
    let reply_answer = last_message(&replied);
    eprintln!("IMAGE second number {second}, answer {reply_answer:?}");
    real.save("final-image-3-replied");
    assert_eq!(replied.turns[1].images.len(), 1, "{replied:#?}");
    assert_eq!(replied.turns[1].kind, gitez_agent_thread::TurnKind::Reply);
    // Every PNG's base64 starts so: none in a transcript, a log, the
    // thread's record or GitEZ's data folder.
    let thread_dir = Store::new(&real.common_dir).thread_dir(&key.id);
    for dir in [thread_dir.as_path(), real.data_dir.as_path()] {
        let found: Vec<_> = files_containing(dir, "iVBORw0KGgo");
        assert!(found.is_empty(), "image data in {found:#?}");
    }
    let runs = std::fs::read_dir(thread_dir.join("runs"))
        .expect("runs")
        .count();
    eprintln!(
        "IMAGE transcripts {runs}, images kept {:?}",
        std::fs::read_dir(thread_dir.join("images"))
            .expect("images")
            .map(|entry| entry.expect("entry").file_name())
            .collect::<Vec<_>>()
    );
    assert!(answer.contains(&first.to_string()), "first: {answer:?}");
    assert!(
        reply_answer.contains(&second.to_string()),
        "second: {reply_answer:?}"
    );
}
