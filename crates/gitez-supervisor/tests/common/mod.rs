//! Helpers shared by the integration tests: a temporary repository with a
//! data folder (both with a space in their path, as GitEZ's has on macOS),
//! a fake `claude` that prints canned stream-JSON, and ways to run the
//! supervisor in-process or as its own process.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use gitez_agent::{ClaudeCode, Registry};
use gitez_agent_thread::{AgentThread, AgentThreadId, NewAgentThread, Start, Store, Timestamp};
use gitez_sandbox::Passthrough;
use gitez_settings::{AgentEntry, Settings};
use gitez_supervisor::{Command, Environment, Launch, Secrets};

/// The fake agent. It logs its arguments to `$FAKE_LOG/calls` (one line
/// per run) and its standard input to `$FAKE_LOG/stdin-<run>` (from 1),
/// reads the prompt from stdin and acts on a keyword in it.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
log="$FAKE_LOG"
sid=""
prev=""
for arg in "$@"; do
  case "$prev" in --session-id|--resume) sid="$arg" ;; esac
  prev="$arg"
done
printf '%s\n' "$*" >> "$log/calls"
prompt=$(cat)
run=$(wc -l < "$log/calls" | tr -d ' ')
printf '%s' "$prompt" > "$log/stdin-$run"
say() { printf '%s\n' "$1"; }
init() { say '{"type":"system","subtype":"init","session_id":"'"$sid"'","model":"fake-model"}'; }
tool() { say '{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"'"$1"'"}}]}}'; }
text() { say '{"type":"assistant","message":{"content":[{"type":"text","text":"'"$1"'"}]}}'; }
result() { say '{"type":"result","subtype":"success","is_error":false,"result":"'"$1"'","session_id":"'"$sid"'","num_turns":1,"duration_ms":5,"total_cost_usd":0.01}'; }
once() { if [ -e "$log/once-$1" ]; then return 1; fi; : > "$log/once-$1"; return 0; }
case "$prompt" in
  *TITLED*)
    init; tool "git commit"
    echo hello > greeting.txt && git add greeting.txt && git commit --quiet -m "Agent: greet" >&2
    echo left > left.txt
    result '# Make the greeting friendlier\n\nThe greeting is warmer now.' ;;
  *TWO-COMMITS*)
    init; tool "git commit"
    echo one > one.txt && git add one.txt && git commit --quiet -m "  \"Add one.txt.\"  " >&2
    echo two > two.txt && git add two.txt && git commit --quiet -m "Add two.txt" >&2
    result "Added two files." ;;
  *RETITLE*)
    init; tool "git commit"
    echo third >> agent.txt && git add agent.txt && git commit --quiet -m "Agent: third line" >&2
    result '# Rename everything\n\nAdded a third line.' ;;
  *COMMIT-AGAIN*)
    init; tool "git commit"
    echo second >> agent.txt && git add agent.txt && git commit --quiet -m "Agent: second line" >&2
    result "Added a second line." ;;
  *COMMIT*)
    init; tool "git commit"
    echo first > agent.txt && git add agent.txt && git commit --quiet -m "Agent: first line" >&2
    text "Committed agent.txt."
    result "Wrote agent.txt and committed it." ;;
  *LEAVE*)
    init; tool "touch left.txt"; echo left > left.txt
    result "Left a file for GitEZ." ;;
  *FAIL-NO-INIT*)
    if once fail-no-init; then echo "boom: the agent fell over" >&2; exit 1; fi
    init; result "Fine this time." ;;
  *FAIL-AFTER-INIT*)
    if once fail-after-init; then init; echo "boom after init" >&2; exit 1; fi
    init; result "Fine this time." ;;
  *SLEEP*)
    init; tool "sleep"
    trap 'echo trapped > "$log/trapped"; say "{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"result\":\"\",\"session_id\":\"$sid\"}"; exit 0' INT
    while [ ! -e "$log/release" ]; do sleep 0.1; done
    result "Released." ;;
  *LIMIT*)
    if once limit; then
      init; reset=$(( $(date +%s) + 2 ))
      say '{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":'"$reset"'}}'
      say '{"type":"result","subtype":"success","is_error":true,"result":"Claude AI usage limit reached|'"$reset"'","session_id":"'"$sid"'"}'
      exit 1
    fi
    init; result "Continued after the limit." ;;
  *SEE-IMAGE*)
    # Echoes the message it was given, as Claude Code does with
    # --replay-user-messages, and an image a tool read.
    init; say "$prompt"
    say '{"type":"user","message":{"content":[{"type":"tool_result","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"'"$(head -c 3000 /dev/zero | tr '\0' 'Q')"'"}}]}]}}'
    result "I see the image." ;;
  *SECRET*)
    init; text "The secret is $DEMO_SECRET"
    result "Saw $DEMO_SECRET in my environment." ;;
  *MOVE*)
    init; tool "git update-ref"; git update-ref refs/heads/other HEAD
    result "Moved another branch." ;;
  *)
    init; result "The answer is 42." ;;
esac
"#;

pub const POLL: Duration = Duration::from_millis(20);

/// `git <args>` in `dir` with a fixed identity and no user or system
/// config; returns its trimmed output.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let output = Process::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("utf-8 output")
        .trim()
        .to_owned()
}

pub struct Fixture {
    _dir: tempfile::TempDir,
    pub base: PathBuf,
    /// The main worktree.
    pub root: PathBuf,
    /// `…/Application Support/GitEZ`.
    pub data_dir: PathBuf,
    /// Where the fake agent logs.
    pub fake_log: PathBuf,
}

impl Fixture {
    pub fn new() -> Self {
        Self::with_limit(3)
    }

    /// A fixture whose settings allow `max_running` agents at once.
    pub fn with_limit(max_running: u32) -> Self {
        let dir = tempfile::tempdir().expect("temporary directory");
        let base = dir.path().canonicalize().expect("canonical temp dir");
        let root = base.join("my repo");
        let data_dir = base.join("Application Support").join("GitEZ");
        let fake_log = base.join("fake log");
        let bin = base.join("bin");
        for folder in [&root, &data_dir, &fake_log, &bin] {
            std::fs::create_dir_all(folder).expect("create folder");
        }
        git(&root, &["init", "--quiet", "--initial-branch=main"]);
        let hooks = base.join("no hooks");
        std::fs::create_dir_all(&hooks).expect("create hooks folder");
        for (key, value) in [
            ("user.name", "Test"),
            ("user.email", "test@example.com"),
            ("commit.gpgsign", "false"),
            ("core.hooksPath", hooks.to_str().expect("utf-8 path")),
        ] {
            git(&root, &["config", key, value]);
        }
        std::fs::write(root.join("README.md"), "hello\n").expect("write");
        std::fs::write(root.join(".gitignore"), "setup-ran\n").expect("write");
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "--quiet", "-m", "First"]);

        let claude = bin.join("claude");
        write_script(&claude, FAKE_CLAUDE);
        let mut settings = Settings::default();
        settings.threads.max_running = max_running;
        settings.agents.insert(
            ClaudeCode::ID.to_owned(),
            AgentEntry {
                program: Some(claude),
                config_dir: Some(base.join("claude config")),
                env: BTreeMap::from([(
                    "FAKE_LOG".to_owned(),
                    fake_log.to_string_lossy().into_owned(),
                )]),
                ..AgentEntry::default()
            },
        );
        settings
            .save(&Settings::path(&data_dir))
            .expect("save settings");
        Self {
            _dir: dir,
            base,
            root,
            data_dir,
            fake_log,
        }
    }

    pub fn common_dir(&self) -> PathBuf {
        self.root.join(".git")
    }

    pub fn store(&self) -> Store {
        Store::new(&self.common_dir())
    }

    pub fn head(&self) -> String {
        git(&self.root, &["rev-parse", "HEAD"])
    }

    pub fn tip(&self, branch: &str) -> String {
        git(&self.root, &["rev-parse", &format!("refs/heads/{branch}")])
    }

    pub fn set_setup_command(&self, command: &str) {
        git(&self.root, &["config", "gitez.setupCommand", command]);
    }

    /// A queued agent thread for `prompt`, from `main`.
    pub fn create(&self, prompt: &str) -> AgentThread {
        self.create_with_secrets(prompt, &[])
    }

    pub fn create_with_secrets(&self, prompt: &str, secrets: &[&str]) -> AgentThread {
        self.create_with(prompt, secrets, &[])
    }

    /// A queued agent thread for `prompt` with `images`, from `main`.
    pub fn create_with_images(
        &self,
        prompt: &str,
        images: &[gitez_agent_thread::PreparedImage],
    ) -> AgentThread {
        self.create_with(prompt, &[], images)
    }

    fn create_with(
        &self,
        prompt: &str,
        secrets: &[&str],
        images: &[gitez_agent_thread::PreparedImage],
    ) -> AgentThread {
        self.store()
            .create_with_images(
                NewAgentThread {
                    prompt: prompt.to_owned(),
                    agent: ClaudeCode::ID.to_owned(),
                    model: Some("haiku".to_owned()),
                    start: Start {
                        branch: "main".to_owned(),
                        commit: self.head(),
                    },
                    network: Default::default(),
                    secrets: secrets.iter().map(|name| (*name).to_owned()).collect(),
                },
                images,
                &self.data_dir,
                "my repo",
                |_| false,
                Timestamp::now(),
            )
            .expect("create thread")
    }

    /// The in-process environment: no sandbox, fast polling.
    pub fn environment(&self) -> Environment {
        Environment {
            data_dir: self.data_dir.clone(),
            settings_path: Settings::path(&self.data_dir),
            agents: Registry::with(vec![Arc::new(ClaudeCode)]),
            sandbox: Arc::new(Passthrough),
            stop: Arc::new(AtomicBool::new(false)),
            poll: POLL,
        }
    }

    /// Runs the supervisor's pipeline in this process.
    pub fn run(&self, id: &AgentThreadId, command: Command) -> AgentThread {
        gitez_supervisor::run(
            &self.environment(),
            &self.common_dir(),
            id,
            command,
            Secrets::default(),
        )
        .expect("run the pipeline")
    }

    pub fn load(&self, id: &AgentThreadId) -> AgentThread {
        self.store().load(id).expect("load thread")
    }

    /// What the fake agent read on standard input in run `run` (from 1).
    pub fn stdin(&self, run: usize) -> String {
        std::fs::read_to_string(self.fake_log.join(format!("stdin-{run}")))
            .unwrap_or_else(|error| panic!("no standard input of run {run}: {error}"))
    }

    /// The fake agent's command lines, one per run.
    pub fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.fake_log.join("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    pub fn thread_dir(&self, id: &AgentThreadId) -> PathBuf {
        self.store().thread_dir(id)
    }

    /// A script that runs the `gitez-supervisor` binary with this
    /// fixture's data folder and no sandbox (debug builds only), so
    /// `launch` can start it without changing this process's environment.
    pub fn supervisor_exe(&self) -> PathBuf {
        let path = self.base.join("bin").join("supervise");
        if !path.exists() {
            write_script(
                &path,
                &format!(
                    "#!/bin/sh\nexport GITEZ_DATA_DIR='{}'\nexport GITEZ_TEST_SANDBOX=passthrough\n\
                     exec '{}' \"$@\"\n",
                    self.data_dir.display(),
                    env!("CARGO_BIN_EXE_gitez-supervisor"),
                ),
            );
        }
        path
    }

    /// A script that runs `exe` (a `gitez` binary, run as `gitez supervise
    /// …`) with this fixture's data folder and `GITEZ_TEST_SANDBOX=passthrough`.
    pub fn gitez_exe(&self, exe: &Path) -> PathBuf {
        let path = self.base.join("bin").join("gitez-supervise");
        write_script(
            &path,
            &format!(
                "#!/bin/sh\nexport GITEZ_DATA_DIR='{}'\nexport GITEZ_TEST_SANDBOX=passthrough\n\
                 exec '{}' supervise \"$@\"\n",
                self.data_dir.display(),
                exe.display(),
            ),
        );
        path
    }

    /// Starts a supervisor process for `id`, as the GUI does.
    pub fn launch(&self, id: &AgentThreadId, command: Command, secrets: &Secrets) -> u32 {
        let exe = self.supervisor_exe();
        gitez_supervisor::launch(Launch {
            exe: &exe,
            prefix: &[],
            command,
            common_dir: &self.common_dir(),
            thread: id,
            secrets,
        })
        .expect("launch the supervisor")
    }

    /// Waits until `done` holds for the thread, or panics with the
    /// supervisor's log.
    pub fn wait_for(
        &self,
        id: &AgentThreadId,
        timeout: Duration,
        done: impl Fn(&AgentThread) -> bool,
    ) -> AgentThread {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(thread) = self.store().load(id)
                && done(&thread)
            {
                return thread;
            }
            if Instant::now() > deadline {
                panic!(
                    "timed out; thread: {:#?}\nsupervisor.log:\n{}",
                    self.store().load(id).ok(),
                    self.supervisor_log(id)
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn supervisor_log(&self, id: &AgentThreadId) -> String {
        std::fs::read_to_string(self.thread_dir(id).join("supervisor.log")).unwrap_or_default()
    }

    /// The agent's transcript lines of the thread's runs, oldest first.
    pub fn transcripts(
        &self,
        id: &AgentThreadId,
    ) -> Vec<(gitez_agent_thread::RunHeader, Vec<String>)> {
        let store = self.store();
        store
            .runs(id)
            .expect("list runs")
            .iter()
            .map(|path| store.read_run(path).expect("read run"))
            .collect()
    }
}

pub fn write_script(path: &Path, body: &str) {
    std::fs::write(path, body).expect("write script");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

/// Whether a thread is no longer active.
pub fn is_done(thread: &AgentThread) -> bool {
    !thread.state.is_active()
}
