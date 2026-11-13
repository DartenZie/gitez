use super::*;

#[test]
fn events_of_a_run_are_parsed() {
    let lines = [
        r#"{"type":"system","subtype":"hook_started","hook_id":"x"}"#,
        r#"{"type":"system","subtype":"init","cwd":"/r","session_id":"0b7c","model":"claude-opus-5-5","tools":["Bash"]}"#,
        r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test\n--quiet","description":"Run tests"}}]}}"#,
        r#"{"type":"user","message":{"content":[{"tool_use_id":"t1","type":"tool_result","content":"ok","is_error":false}]}}"#,
        r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t2","name":"Edit","input":{"file_path":"src/a.rs","old_string":"a","new_string":"b"}}]}}"#,
        r#"{"type":"user","message":{"content":[{"tool_use_id":"t2","type":"tool_result","content":[{"type":"text","text":"File has not been read yet.\nRead it first."}],"is_error":true}]}}"#,
        r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":""}]}}"#,
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Done.\n"}]}}"#,
        r#"{"type":"rate_limit_event","rate_limit_info":{}}"#,
        r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1794545400,"rateLimitType":"five_hour"}}"#,
        r#"{"type":"result","subtype":"success","is_error":false,"result":"Done.","session_id":"0b7c","total_cost_usd":0.25,"num_turns":4,"duration_ms":4729}"#,
        "not json",
    ];
    let events: Vec<AgentEvent> = lines.iter().filter_map(|line| parse(line)).collect();
    assert_eq!(
        events,
        [
            AgentEvent::Started {
                session_id: Some("0b7c".to_owned()),
                model: Some("claude-opus-5-5".to_owned())
            },
            AgentEvent::Tool {
                name: "Bash".to_owned(),
                detail: "cargo test".to_owned()
            },
            AgentEvent::Tool {
                name: "Edit".to_owned(),
                detail: "src/a.rs".to_owned()
            },
            AgentEvent::ToolFailed("File has not been read yet.".to_owned()),
            AgentEvent::Text("Done.".to_owned()),
            AgentEvent::Finished(Finished {
                success: true,
                text: "Done.".to_owned(),
                session_id: Some("0b7c".to_owned()),
                cost_usd: Some(0.25),
                turns: Some(4),
                duration_ms: Some(4729),
                usage_limit: None,
            }),
        ]
    );
}

#[test]
fn an_init_without_a_session_id_still_starts() {
    let line = r#"{"type":"system","subtype":"init","model":"claude-haiku-5-5"}"#;
    assert_eq!(
        parse(line),
        Some(AgentEvent::Started {
            session_id: None,
            model: Some("claude-haiku-5-5".to_owned())
        })
    );
}

#[test]
fn an_error_result_is_not_a_success() {
    let line = r#"{"type":"result","subtype":"error_max_turns","is_error":true}"#;
    let Some(AgentEvent::Finished(finished)) = parse(line) else {
        panic!("a result");
    };
    assert!(!finished.success);
    assert_eq!(finished.text, "");
    assert_eq!(finished.session_id, None);
    assert_eq!(finished.usage_limit, None);
}

#[test]
fn a_rejected_rate_limit_event_is_the_usage_limit() {
    let line = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1794545400,"rateLimitType":"five_hour"}}"#;
    assert_eq!(
        parse(line),
        Some(AgentEvent::UsageLimit(UsageLimit {
            resets_at: Some(1_794_545_400)
        }))
    );
    let without_time = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected"}}"#;
    assert_eq!(
        parse(without_time),
        Some(AgentEvent::UsageLimit(UsageLimit { resets_at: None }))
    );
}

#[test]
fn a_usage_limit_result_has_its_reset_time() {
    let line = r#"{"type":"result","subtype":"success","is_error":true,"result":"Claude AI usage limit reached|1794545400","session_id":"s-1"}"#;
    let Some(AgentEvent::Finished(finished)) = parse(line) else {
        panic!("a result");
    };
    assert!(!finished.success);
    assert_eq!(finished.text, "Claude AI usage limit reached");
    assert_eq!(finished.session_id.as_deref(), Some("s-1"));
    assert_eq!(
        finished.usage_limit,
        Some(UsageLimit {
            resets_at: Some(1_794_545_400)
        })
    );

    let no_time = r#"{"type":"result","subtype":"success","is_error":true,"result":"You've hit your Usage Limit."}"#;
    let Some(AgentEvent::Finished(finished)) = parse(no_time) else {
        panic!("a result");
    };
    assert_eq!(finished.text, "You've hit your Usage Limit.");
    assert_eq!(finished.usage_limit, Some(UsageLimit { resets_at: None }));

    // Not an error: the words alone are not the limit.
    let mention = r#"{"type":"result","subtype":"success","is_error":false,"result":"Documented the usage limit|42"}"#;
    let Some(AgentEvent::Finished(finished)) = parse(mention) else {
        panic!("a result");
    };
    assert_eq!(finished.usage_limit, None);
    assert_eq!(finished.text, "Documented the usage limit|42");
}

/// A config whose program and configuration folder exist, so nothing is
/// looked up on this machine.
fn config(dir: &tempfile::TempDir) -> AgentConfig {
    let program = dir.path().join("claude");
    std::fs::write(&program, "").expect("write program");
    AgentConfig {
        program: Some(program),
        config_dir: Some(dir.path().join("config")),
        env: vec![
            ("FOO".to_owned(), "1".to_owned()),
            ("BAR".to_owned(), "2".to_owned()),
        ],
        args: vec!["--add-dir".to_owned(), "/x".to_owned()],
    }
}

fn run<'a>(conversation: Conversation<'a>, cwd: &'a Path) -> RunSpec<'a> {
    RunSpec {
        prompt: "Fix the bug",
        cwd,
        model: Some("haiku"),
        conversation,
        system_prompt: Some("Commit as you go."),
    }
}

#[test]
fn a_new_conversation_has_its_arguments_in_order() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let config = config(&dir);
    let command = ClaudeCode
        .command(
            &config,
            &run(Conversation::New { session_id: "s-1" }, dir.path()),
        )
        .expect("command");
    assert_eq!(command.program, dir.path().join("claude"));
    assert_eq!(
        command.args,
        [
            "--print",
            "--output-format",
            "stream-json",
            "--verbose",
            "--permission-mode",
            "bypassPermissions",
            "--settings",
            r#"{"sandbox":{"enabled":false}}"#,
            "--model",
            "haiku",
            "--session-id",
            "s-1",
            "--append-system-prompt",
            "Commit as you go.",
            "--add-dir",
            "/x",
        ]
    );
    assert_eq!(command.stdin.as_deref(), Some("Fix the bug"));
    assert_eq!(command.cwd, dir.path());
    assert_eq!(
        command.env_remove,
        [
            "CLAUDECODE",
            "CLAUDE_CODE_ENTRYPOINT",
            "CLAUDE_CODE_SSE_PORT"
        ]
    );
    let names: Vec<&str> = command.env.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["CLAUDE_CONFIG_DIR", "PATH", "FOO", "BAR"]);
    assert_eq!(
        command.env[0].1,
        dir.path().join("config").to_string_lossy()
    );
    // The agent's own folder is on its PATH.
    assert!(std::env::split_paths(&command.env[1].1).any(|entry| entry == dir.path()));
}

#[test]
fn a_resumed_conversation_without_extras() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let mut config = config(&dir);
    config.args.clear();
    config.env.clear();
    let mut run = run(Conversation::Resume { session_id: "s-1" }, dir.path());
    run.model = Some("");
    run.system_prompt = None;
    let command = ClaudeCode.command(&config, &run).expect("command");
    assert_eq!(
        command.args[ARGS.len() + SANDBOX_OFF.len()..],
        ["--resume", "s-1"]
    );
    run.model = None;
    let command = ClaudeCode.command(&config, &run).expect("command");
    assert_eq!(
        command.args[ARGS.len() + SANDBOX_OFF.len()..],
        ["--resume", "s-1"]
    );
}

#[test]
fn the_review_panel_keeps_claude_codes_own_sandbox() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let mut command = ClaudeCode
        .command(
            &config(&dir),
            &run(Conversation::New { session_id: "s-1" }, dir.path()),
        )
        .expect("command");
    let before = command.args.len();
    ClaudeCode::keep_own_sandbox(&mut command);
    assert_eq!(command.args.len(), before - 2);
    assert!(!command.args.iter().any(|arg| arg == "--settings"));
    assert_eq!(command.args[..ARGS.len()], ARGS);
    assert_eq!(command.args[ARGS.len()..][..2], ["--model", "haiku"]);
}

#[test]
fn a_configured_program_must_be_a_file() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let config = AgentConfig {
        program: Some(dir.path().to_owned()),
        ..AgentConfig::default()
    };
    let error = ClaudeCode.locate(&config).expect_err("a folder");
    assert!(matches!(error, Error::NotAProgram { .. }), "{error}");
}

#[test]
fn the_config_dir_comes_from_the_first_place_that_sets_it() {
    let home = Path::new("/Users/me");
    let shell = || Some(OsString::from("/from/shell"));
    let resolve = |configured: Option<&str>, environment: Option<&str>| {
        resolve_config_dir(
            configured.map(Path::new),
            environment.map(OsString::from),
            shell,
            Some(home),
        )
    };
    assert_eq!(
        resolve(Some("~/.config/claude"), Some("/from/env")),
        ConfigDir {
            path: "/Users/me/.config/claude".into(),
            source: ConfigDirSource::Settings
        }
    );
    assert_eq!(
        resolve(Some(""), Some("/from/env")),
        ConfigDir {
            path: "/from/env".into(),
            source: ConfigDirSource::Environment
        }
    );
    assert_eq!(
        resolve(None, Some("")),
        ConfigDir {
            path: "/from/shell".into(),
            source: ConfigDirSource::LoginShell
        }
    );
    assert_eq!(
        resolve_config_dir(None, None, || None, Some(home)),
        ConfigDir {
            path: "/Users/me/.claude".into(),
            source: ConfigDirSource::Default
        }
    );
    // The login shell is not asked when something else set it.
    resolve_config_dir(
        None,
        Some("/from/env".into()),
        || panic!("asked the login shell"),
        Some(home),
    );
}

#[test]
fn folder_names_match_claude_codes() {
    assert_eq!(
        sanitise("/private/tmp/claude-501/-Users-me/spike/wt/test"),
        "-private-tmp-claude-501--Users-me-spike-wt-test"
    );
    // One `-` per UTF-16 unit, as JavaScript counts.
    assert_eq!(sanitise("/private/tmp/a b/é😀"), "-private-tmp-a-b----");
    // Computed with Claude Code's own function in Node.
    let long = format!(
        "/private/tmp/{}wörktree",
        "very-long-folder-name/".repeat(10)
    );
    assert_eq!(
        sanitise(&long),
        format!(
            "-private-tmp-{}very-long-f-2zl7fj",
            "very-long-folder-name-".repeat(8)
        )
    );
}

#[test]
fn a_run_may_write_only_its_own_folders() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let config = config(&dir);
    let config_dir = dir.path().join("config");
    let cwd = dir.path().join("wt");
    std::fs::create_dir(&cwd).expect("worktree");
    let run = run(Conversation::New { session_id: "s-1" }, &cwd);
    let needs = ClaudeCode.needs_for_run(&config, &run);
    let name = sanitise(&std::fs::canonicalize(&cwd).expect("real").to_string_lossy());
    for expected in [
        config_dir.join("projects").join(&name),
        config_dir.join("session-env/s-1"),
        config_dir.join("file-history/s-1"),
    ] {
        assert!(needs.write_dirs.contains(&expected), "{expected:?}");
    }
    assert!(needs.write_files.is_empty());
    // Nothing in the configuration folder but those.
    let inside: Vec<_> = needs
        .write_dirs
        .iter()
        .filter(|dir| dir.starts_with(&config_dir))
        .collect();
    assert!(inside.len() <= 4, "{inside:?}");
    assert!(!needs.write_dirs.contains(&config_dir));
    assert_eq!(needs.hosts, ["api.anthropic.com", "platform.claude.com"]);
    #[cfg(unix)]
    {
        let uid = rustix::process::getuid().as_raw();
        let tmp = std::fs::canonicalize("/tmp").expect("tmp");
        assert!(
            needs
                .write_dirs
                .contains(&tmp.join(format!("claude-{uid}")).join(&name))
        );
        assert!(needs.write_dirs.contains(&tmp.join("claude-*-cwd")));
    }

    // An id that is not a UUID-like name opens nothing.
    let odd = RunSpec {
        conversation: Conversation::New {
            session_id: "../..",
        },
        ..run
    };
    let needs = ClaudeCode.needs_for_run(&config, &odd);
    assert!(!needs.write_dirs.iter().any(|dir| dir.ends_with("..")));
    assert!(
        !needs
            .write_dirs
            .iter()
            .any(|dir| dir.starts_with(config_dir.join("session-env")))
    );
}

#[test]
fn claude_codes_temp_folder_follows_its_setting() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let mut config = config(&dir);
    let tmp = dir.path().join("tmp");
    std::fs::create_dir(&tmp).expect("tmp");
    config.env.push((
        "CLAUDE_CODE_TMPDIR".to_owned(),
        tmp.to_string_lossy().into_owned(),
    ));
    let needs = ClaudeCode.needs(&config);
    let real = std::fs::canonicalize(&tmp).expect("real");
    assert!(needs.write_dirs.contains(&real.join("claude-*-cwd")));
}

#[test]
fn the_login_status_is_read_from_the_logged_in_field() {
    assert_eq!(
        logged_in(br#"{"loggedIn":true,"authMethod":"claude.ai"}"#),
        Some(true)
    );
    assert_eq!(
        logged_in(b"warning: something\n{\"loggedIn\":false}\n"),
        Some(false)
    );
    assert_eq!(logged_in(b"Not logged in"), None);
}

#[test]
fn the_first_model_is_the_agent_default() {
    assert_eq!(ClaudeCode.models()[0], ("", "Agent default"));
    assert!(
        ClaudeCode
            .models()
            .iter()
            .any(|(alias, _)| *alias == "haiku")
    );
}
