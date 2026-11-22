use super::*;
use crate::ImageInput;

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
        images: &[],
        cwd,
        shared_folder: false,
        model: Some("haiku"),
        effort: Some("high"),
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
            "--effort",
            "high",
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
    // A level Claude Code does not know is left out, not passed on.
    run.effort = Some("extreme");
    run.system_prompt = None;
    let command = ClaudeCode.command(&config, &run).expect("command");
    assert_eq!(
        command.args[ARGS.len() + SANDBOX_OFF.len()..],
        ["--resume", "s-1"]
    );
    run.model = None;
    run.effort = None;
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
fn a_run_in_a_shared_folder_may_write_only_its_own_conversation() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let config = config(&dir);
    let config_dir = dir.path().join("config");
    let cwd = dir.path().join("checkout");
    std::fs::create_dir(&cwd).expect("checkout");
    let session = "6f1c2b8e-3a1d-4c55-9b0e-2d7f1e4a9c10";
    let run = RunSpec {
        shared_folder: true,
        ..run(
            Conversation::Resume {
                session_id: session,
            },
            &cwd,
        )
    };
    let needs = ClaudeCode.needs_for_run(&config, &run);
    let name = sanitise(&std::fs::canonicalize(&cwd).expect("real").to_string_lossy());
    let project = config_dir.join("projects").join(&name);
    // Its own transcript, subagents and tool results; never the folder,
    // where the user's own conversations and their memory are.
    assert!(
        needs
            .write_files
            .contains(&project.join(format!("{session}.jsonl")))
    );
    assert!(needs.write_dirs.contains(&project.join(session)));
    for path in needs.write_dirs.iter().chain(&needs.write_files) {
        assert!(
            !project.starts_with(path),
            "{path:?} holds the whole folder"
        );
        assert!(!path.starts_with(project.join("memory")), "{path:?}");
    }
    assert!(needs.create_dirs.contains(&project));
    assert!(
        needs
            .write_dirs
            .contains(&config_dir.join("session-env").join(session))
    );
    #[cfg(unix)]
    {
        let uid = rustix::process::getuid().as_raw();
        let tmp = std::fs::canonicalize("/tmp")
            .expect("tmp")
            .join(format!("claude-{uid}"))
            .join(&name);
        assert!(needs.write_dirs.contains(&tmp.join(session)));
        assert!(!needs.write_dirs.contains(&tmp));
    }

    // Without an id that names a folder safely, nothing of the folder.
    let odd = RunSpec {
        conversation: Conversation::New {
            session_id: "../..",
        },
        ..run
    };
    let needs = ClaudeCode.needs_for_run(&config, &odd);
    assert!(needs.write_files.is_empty());
    assert!(!needs.write_dirs.iter().any(|dir| dir.starts_with(&project)));

    // A thread's own worktree is its own: nothing to make beforehand.
    let own = RunSpec {
        shared_folder: false,
        ..run
    };
    let needs = ClaudeCode.needs_for_run(&config, &own);
    assert!(needs.write_dirs.contains(&project));
    assert!(needs.create_dirs.is_empty());
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
fn every_model_is_named_and_opus_is_the_default() {
    let aliases: Vec<&str> = ClaudeCode
        .models()
        .iter()
        .map(|(alias, _)| *alias)
        .collect();
    assert_eq!(aliases, ["fable", "opus", "sonnet", "haiku"]);
    assert!(aliases.contains(&ClaudeCode.default_model()));
    assert_eq!(ClaudeCode.default_model(), "opus");
}

/// Claude Code's connectors proxy and telemetry intake, which real runs
/// try every time, are optional and never among the hosts the sandbox
/// allows; any other blocked host is a problem.
#[test]
fn claude_codes_own_extra_hosts_are_optional() {
    let dir = tempfile::tempdir().expect("temp dir");
    let needs = ClaudeCode.needs(&config(&dir));
    for optional in ClaudeCode.optional_hosts() {
        assert!(
            !needs.hosts.iter().any(|host| host == optional.host),
            "{} stays blocked",
            optional.host
        );
    }
    let blocked: Vec<String> = [
        "mcp-proxy.anthropic.com",
        "static.crates.io",
        "HTTP-INTAKE.logs.us5.datadoghq.com",
        "logs.us5.datadoghq.com",
        "anthropic.com",
    ]
    .map(str::to_owned)
    .into();
    let split = crate::BlockedHosts::classify(Some(&ClaudeCode), &blocked);
    assert_eq!(
        split.problems,
        [
            "static.crates.io",
            "logs.us5.datadoghq.com",
            "anthropic.com"
        ],
        "exact hosts only: no parent domain or wildcard"
    );
    assert_eq!(
        split.optional,
        [
            (
                "mcp-proxy.anthropic.com",
                "Claude Code's claude.ai connectors, not needed"
            ),
            (
                "HTTP-INTAKE.logs.us5.datadoghq.com",
                "Claude Code's telemetry, not needed"
            ),
        ]
    );
    // An agent GitEZ does not know: every host is a problem.
    let unknown = crate::BlockedHosts::classify(None, &blocked);
    assert_eq!(unknown.problems.len(), blocked.len());
    assert!(unknown.optional.is_empty());
}

#[test]
fn claude_code_accepts_images_with_every_model() {
    for (alias, _) in ClaudeCode.models() {
        assert!(ClaudeCode.accepts_images(Some(alias)), "{alias}");
    }
    assert!(ClaudeCode.accepts_images(None));
    assert!(ClaudeCode.accepts_images(Some("claude-some-future-model")));
}

#[test]
fn a_run_with_images_sends_one_stream_json_message() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let config = config(&dir);
    let plain = ClaudeCode
        .command(
            &config,
            &run(Conversation::Resume { session_id: "s-1" }, dir.path()),
        )
        .expect("command");
    let images = [
        ImageInput {
            media_type: "image/png",
            data: b"\x89PNG one",
        },
        ImageInput {
            media_type: "image/jpeg",
            data: b"\xff\xd8 two",
        },
    ];
    let with_images = RunSpec {
        images: &images,
        ..run(Conversation::Resume { session_id: "s-1" }, dir.path())
    };
    let command = ClaudeCode.command(&config, &with_images).expect("command");
    // The same arguments, with the input format after GitEZ's own.
    let at = ARGS.len() + SANDBOX_OFF.len();
    assert_eq!(command.args[..at], plain.args[..at]);
    assert_eq!(command.args[at..at + 2], ["--input-format", "stream-json"]);
    assert_eq!(command.args[at + 2..], plain.args[at..]);
    assert!(
        command
            .args
            .windows(2)
            .any(|pair| pair == ["--resume", "s-1"])
    );

    let stdin = command.stdin.expect("standard input");
    assert!(stdin.ends_with('\n'));
    assert_eq!(stdin.lines().count(), 1);
    let message: Value = serde_json::from_str(stdin.trim_end()).expect("JSON");
    assert_eq!(message["type"], "user");
    assert_eq!(message["parent_tool_use_id"], Value::Null);
    assert_eq!(message["message"]["role"], "user");
    let content = message["message"]["content"]
        .as_array()
        .expect("content blocks");
    assert_eq!(content.len(), 3);
    let engine = base64::engine::general_purpose::STANDARD;
    for (block, image) in content.iter().zip(&images) {
        assert_eq!(block["type"], "image");
        assert_eq!(block["source"]["type"], "base64");
        assert_eq!(block["source"]["media_type"], image.media_type);
        let data = block["source"]["data"].as_str().expect("data");
        assert_eq!(engine.decode(data).expect("base64"), image.data);
    }
    assert_eq!(content[2], json!({"type": "text", "text": "Fix the bug"}));
}

#[test]
fn an_image_without_words_has_no_text_block() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let images = [ImageInput {
        media_type: "image/gif",
        data: b"GIF89a",
    }];
    let run = RunSpec {
        prompt: "  \n",
        images: &images,
        ..run(Conversation::New { session_id: "s-1" }, dir.path())
    };
    let command = ClaudeCode.command(&config(&dir), &run).expect("command");
    let message: Value =
        serde_json::from_str(command.stdin.as_deref().expect("stdin").trim()).expect("JSON");
    let content = message["message"]["content"].as_array().expect("content");
    assert_eq!(content.len(), 1);
    assert_eq!(content[0]["type"], "image");
}

#[test]
fn a_transcript_keeps_no_image_data() {
    let data = "iVBORw0KGgo".repeat(100);
    let line = format!(
        r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","content":[{{"type":"image","source":{{"type":"base64","media_type":"image/png","data":"{data}"}}}}]}}]}}}}"#
    );
    let kept = ClaudeCode.transcript_line(&line);
    assert!(!kept.contains(&data[..OMIT_FROM]), "{kept}");
    let event: Value = serde_json::from_str(&kept).expect("still JSON");
    assert_eq!(
        event["message"]["content"][0]["content"][0]["source"]["data"],
        IMAGE_OMITTED
    );
    // Cut short by the line limit, it loses the data too.
    let cut = &line[..line.len() - 40];
    let kept = ClaudeCode.transcript_line(cut);
    assert!(!kept.contains(&data[..OMIT_FROM]));
    assert!(kept.starts_with(r#"{"type":"user""#));

    // A small image is left out too; other data stays.
    let small = r#"{"source":{"type":"base64","media_type":"image/gif","data":"R0lGOD"}}"#;
    // (The order of its keys depends on serde_json's features in the
    // build, so the line is compared as JSON.)
    let kept: Value = serde_json::from_str(&ClaudeCode.transcript_line(small)).expect("JSON");
    assert_eq!(
        kept,
        serde_json::json!({"source": {"type": "base64", "media_type": "image/gif", "data": IMAGE_OMITTED}})
    );
    // Lines without image data stay as they are, borrowed.
    for plain in [
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"\"data\":\"short\""}]}}"#,
        r#"{"type":"user","data":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"}"#,
        r#"{"type":"result","result":"Done."}"#,
    ] {
        assert!(
            matches!(ClaudeCode.transcript_line(plain), Cow::Borrowed(_)),
            "{plain}"
        );
    }
}
