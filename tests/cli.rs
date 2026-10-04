//! The process itself: build identification (--version, /health), the startup banner, configuration errors that stop
//! the start with a clear message, the pre-0.0.1 configuration layout, environment-only setups, and a responses store
//! that cannot be created.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::process::Output;

use common::*;
use serde_json::json;

const LEGACY_TOML: &str = r#"
default = "gpt-4.1"

[stackspot]
realm = "acme"
client_id = "cid"
client_secret = "secret"
idm_base_url = "{upstream}"
agent_base_url = "{upstream}/v1/agent"

[limits]
requests_per_minute = 33

[[agents]]
name = "gpt-5.1"
agent_id = "A51"

[[agents]]
name = "gpt-4.1"
agent_id = "${MY_41}"
aliases = ["claude-haiku-4-5"]
"#;

/// One short-lived run in a fresh working directory with that config (or none): --version, or a start that must fail.
fn run(args: &[&str], toml: Option<&str>, env: &[(&str, &str)]) -> Output {
    let dir = tempfile::tempdir().unwrap();
    if let Some(t) = toml {
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        std::fs::write(
            dir.path().join("config/midir.toml"),
            t.replace("{responses_dir}", &dir.path().join("responses").to_string_lossy()).replace("{upstream}", "http://127.0.0.1:9"),
        )
        .unwrap();
    }
    midir_command(env).args(args).current_dir(dir.path()).output().unwrap()
}

fn out(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

fn version() -> String {
    let o = run(&["--version"], None, &[("MIDIR_BUILD_CHANNEL", "release")]);
    String::from_utf8_lossy(&o.stdout).split_whitespace().last().unwrap().to_string()
}

// ---------------------------------------------------------------- build identification

#[test]
fn the_release_version_is_the_crate_version() {
    assert_eq!(version(), env!("CARGO_PKG_VERSION"));
}

#[test]
fn version_reports_the_build() {
    let v = version();
    let cases: [(&[(&str, &str)], String); 4] = [
        (&[("MIDIR_BUILD_CHANNEL", "release"), ("MIDIR_BUILD_COMMIT", "a817822f00d")], v.clone()),
        (&[("MIDIR_BUILD_CHANNEL", "dev"), ("MIDIR_BUILD_COMMIT", "a817822f00dbeef")], format!("{v}+dev.a817822")),
        (&[("MIDIR_BUILD_CHANNEL", "local"), ("MIDIR_BUILD_COMMIT", "a817822-dirty")], format!("{v}+local.a817822.dirty")),
        (&[("MIDIR_BUILD_CHANNEL", "dev")], format!("{v}+dev")),
    ];
    for (env, expected) in cases {
        let o = run(&["--version"], None, env);
        assert_eq!(String::from_utf8_lossy(&o.stdout).trim(), format!("midir {expected}"), "{env:?}");
    }
}

#[test]
fn health_and_ready_report_the_baked_build() {
    let rig = Rig::with(
        MIDIR_TOML,
        &[("MIDIR_BUILD_CHANNEL", "dev"), ("MIDIR_BUILD_COMMIT", "a817822f00d-dirty"), ("MIDIR_BUILD_DATE", "2026-10-03T12:00:00Z")],
    );
    let v = version();
    for path in ["/health", "/ready"] {
        let body = rig.http.get(path).json();
        assert_eq!(body["version"], format!("{v}+dev.a817822.dirty"));
        assert_eq!(
            body["build"],
            json!({"version": body["version"], "channel": "dev", "commit": "a817822", "dirty": true, "date": "2026-10-03T12:00:00Z"})
        );
    }
}

#[test]
fn startup_banner() {
    let rig = Rig::with(MIDIR_TOML, &[("MIDIR_NO_BANNER", ""), ("MIDIR_BUILD_CHANNEL", "dev"), ("MIDIR_BUILD_COMMIT", "abc1234")]);
    let log = rig.server.logs();
    let lines: Vec<&str> = log.lines().filter(|l| l.contains('│')).collect();
    assert!(!log.contains('\t'));
    assert_eq!(lines.len(), 3, "{log}");
    assert!(lines[1].contains("M  I  D  I  R"));
    let rule: std::collections::BTreeSet<usize> = lines.iter().map(|l| l.chars().position(|c| c == '│').unwrap()).collect();
    assert_eq!(rule.len(), 1); // one straight vertical rule
    assert!(lines[0].ends_with("+dev.abc1234 · development build") && lines[1].ends_with("an LLM gateway for agent platforms"), "{log}");
}

#[test]
fn help_lists_the_options() {
    let o = run(&["--help"], None, &[]);
    let text = out(&o);
    assert!(o.status.success());
    for opt in ["--host", "--port", "--debug", "--version", "--healthcheck", "MIDIR_CONFIG"] {
        assert!(text.contains(opt), "{opt}: {text}");
    }
}

#[test]
fn healthcheck_exit_code() {
    let rig = Rig::new();
    let port = rig.server.port.to_string();
    assert!(run(&["--healthcheck", "--port", &port], None, &[]).status.success());
    assert!(!run(&["--healthcheck", "--port", &free_port().to_string()], None, &[]).status.success());
}

// ---------------------------------------------------------------- configuration errors stop the start

#[test]
fn invalid_configuration_is_a_clear_error() {
    let bad_model = format!("{MIDIR_TOML}\n[[models]]\n");
    let cases = [
        (MIDIR_TOML.replace("target = \"AGENT41\"", "target = \"${NOPE}\""), "'target' is empty"),
        (format!("{bad_model}name = \"flex\"\ntarget = \"X\"\n"), "unique"),
        (format!("{bad_model}name = \"z\"\ntarget = \"X\"\nmatch = \"(\"\n"), "invalid 'match'"),
        (format!("{bad_model}name = \"z\"\ntarget = \"X\"\nbackend = \"nope\"\n"), "'backend' must be one of"),
        (MIDIR_TOML.replace("default_model = \"gpt-5.1\"", "default_model = \"nope\""), "not one of the configured models"),
        ("this is = = not toml".to_string(), "cannot read"),
        (MIDIR_TOML.replace("type = \"stackspot\"", "type = \"nope\""), "unknown type \"nope\""),
    ];
    for (toml, message) in cases {
        let o = run(&["--port", "1"], Some(&toml), &[]);
        assert!(!o.status.success() && out(&o).contains(message), "{message}: {}", out(&o));
    }
}

#[test]
fn no_models_at_all_is_a_clear_error() {
    let o = run(&["--port", "1"], None, &[]);
    assert!(!o.status.success() && out(&o).contains("no models configured"), "{}", out(&o));
}

#[test]
fn missing_credentials_message() {
    let o = run(&["--port", "1"], None, &[("STACKSPOT_DEFAULT_AGENT_ID", "x")]);
    assert!(!o.status.success() && out(&o).contains("STACKSPOT_REALM"), "{}", out(&o));
}

// ---------------------------------------------------------------- other ways to configure and to start

#[test]
fn legacy_layout_still_works() {
    let rig = Rig::with(LEGACY_TOML, &[("MY_41", "A41")]);
    let r = rig.http.post("/v1/chat/completions", &json!({"model": "claude-haiku-4-5", "messages": user("hi")}));
    assert_eq!(r.status, 200);
    assert_eq!(rig.upstream.calls().last().unwrap().agent, "A41");
    let health = rig.http.get("/health").json();
    assert_eq!(health["default"], "gpt-4.1");
    assert_eq!(health["backends"]["stackspot"]["queue"]["requests_per_minute"], 33);
    assert!(rig.server.logs().contains("pre-0.0.1 layout"));
}

#[test]
fn environment_overrides_the_file() {
    let rig = Rig::with(MIDIR_TOML, &[("MIDIR_REQUESTS_PER_MINUTE", "42"), ("STACKSPOT_REALM", "from-env")]);
    rig.upstream.add("ok");
    assert_eq!(rig.http.get("/health").json()["backends"]["stackspot"]["queue"]["requests_per_minute"], 42);
    assert_eq!(rig.http.post("/v1/chat/completions", &json!({"model": "gpt-5.1", "messages": user("hi")})).status, 200);
}

#[test]
fn environment_only_setup() {
    let upstream = Upstream::new();
    let agent_base = format!("{}/v1/agent", upstream.url);
    let env = [
        ("STACKSPOT_REALM", "acme"),
        ("STACKSPOT_CLIENT_ID", "cid"),
        ("STACKSPOT_CLIENT_SECRET", "secret"),
        ("STACKSPOT_IDM_BASE_URL", upstream.url.as_str()),
        ("STACKSPOT_AGENT_BASE_URL", agent_base.as_str()),
        ("STACKSPOT_DEFAULT_AGENT_ID", " D "),
        ("STACKSPOT_GPT_5_1_AGENT_ID", "A"),
    ];
    let server = Server::start("", &upstream, &env);
    let http = Http::new(&server.url);
    let models = http.get("/v1/models").json();
    assert!(models["data"].as_array().unwrap().iter().any(|m| m["id"] == "gpt-5.1"));
    for model in ["gpt-5.1", "anything-else"] {
        assert_eq!(http.post("/v1/chat/completions", &json!({"model": model, "messages": user("hi")})).status, 200);
    }
    assert_eq!(upstream.calls().iter().map(|c| c.agent.clone()).collect::<Vec<_>>(), vec!["A", "D"]);
}

#[test]
fn dotenv_in_the_working_directory_is_read_without_overriding_the_environment() {
    let upstream = Upstream::new();
    let toml = MIDIR_TOML.replace("target = \"AGENT41\"", "target = \"${MY_41}\"").replace("target = \"AGENT51\"", "target = \"${MY_51}\"");
    let dir = tempfile::tempdir().unwrap();
    // the .env must exist before the first start: write it next to the config the server will use
    let mut server = Server::start(MIDIR_TOML, &upstream, &[("MY_41", "FROM_ENV")]);
    std::fs::write(
        server.workdir.join("config/midir.toml"),
        toml.replace("{responses_dir}", &dir.path().to_string_lossy()).replace("{upstream}", &upstream.url),
    )
    .unwrap();
    write_file(&server.workdir.join(".env"), "MY_41=FROM_DOTENV\nexport MY_51=\"quoted\" # comment\n");
    server.restart();
    let http = Http::new(&server.url);
    http.post("/v1/chat/completions", &json!({"model": "gpt-4.1", "messages": user("hi")}));
    http.post("/v1/chat/completions", &json!({"model": "gpt-5.1", "messages": user("hi")}));
    let agents: Vec<String> = upstream.calls().iter().map(|c| c.agent.clone()).collect();
    assert_eq!(agents, vec!["FROM_ENV", "quoted"]);
}

#[test]
fn unwritable_store_directory_falls_back_to_memory() {
    // `docker run` without the data volume: the store cannot create its folder; Midir starts anyway.
    let locked = tempfile::tempdir().unwrap();
    std::fs::set_permissions(locked.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
    let toml = MIDIR_TOML
        .replace("responses_dir = \"{responses_dir}\"", &format!("responses_dir = \"{}\"", locked.path().join("data/responses").display()));
    let rig = Rig::with(&toml, &[]);
    let first = rig.http.post("/v1/responses", &json!({"model": "gpt-5.1", "input": "hi"})).json();
    let second = rig.http.post("/v1/responses", &json!({"model": "gpt-5.1", "input": "again", "previous_response_id": first["id"]}));
    std::fs::set_permissions(locked.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(second.status, 200);
    assert!(rig.server.logs().contains("kept in memory only"));
}

// ---------------------------------------------------------------- operation

#[test]
fn unknown_settings_are_reported() {
    let toml = MIDIR_TOML
        .replace("[server]", "[server]\nrequests_per_minut = 5")
        .replace("max_concurrent = 4", "max_concurrent = 4\ncooldown = 1");
    let toml = toml.replace("client_secret = \"secret\"", "client_secret = \"secret\"\nclient_secert = \"typo\"");
    let rig = Rig::with(&toml, &[]);
    let log = rig.server.logs();
    for key in ["server.requests_per_minut", "backends.stackspot.limits.cooldown", "\"client_secert\""] {
        assert!(log.contains(key), "{key}: {log}");
    }
}

#[test]
fn an_api_key_guards_everything_but_health() {
    let rig = Rig::with(MIDIR_TOML, &[("MIDIR_API_KEY", "s3cret")]);
    rig.upstream.add("ok").add("ok");
    let body = json!({"model": "gpt-5.1", "messages": user("hi")});
    let none = rig.http.post("/v1/chat/completions", &body);
    assert_eq!(none.status, 401);
    assert_eq!(none.json()["error"]["code"], "invalid_api_key");
    assert_eq!(rig.http.post_with_headers("/v1/chat/completions", &body, &[("authorization", "Bearer nope")]).status, 401);
    assert_eq!(rig.http.post_with_headers("/v1/chat/completions", &body, &[("authorization", "Bearer s3cret")]).status, 200);
    let anthropic = json!({"model": "gpt-5.1", "max_tokens": 9, "messages": user("hi")});
    let r = rig.http.post("/v1/messages", &anthropic);
    assert_eq!((r.status, r.json()["error"]["type"].clone()), (401, json!("authentication_error")));
    assert_eq!(rig.http.post_with_headers("/v1/messages", &anthropic, &[("x-api-key", "s3cret")]).status, 200);
    assert_eq!(rig.http.get("/health").status, 200);
    assert_eq!(rig.http.get("/v1/models").status, 401);
    assert!(rig.upstream.calls().len() == 2 && !rig.server.logs().contains("s3cret"));
}

#[test]
fn a_stop_lets_running_streams_finish() {
    let mut rig = Rig::new();
    rig.upstream.add(Reply::text("slow but complete").delay(1.5));
    let http = Http::new(&rig.server.url);
    let request =
        std::thread::spawn(move || http.post("/v1/chat/completions", &json!({"model": "gpt-5.1", "messages": user("hi"), "stream": true})));
    std::thread::sleep(std::time::Duration::from_millis(500));
    rig.server.stop(); // SIGTERM while the backend is still thinking
    let r = request.join().unwrap();
    assert_eq!(chat_stream_text(&r.objects()), "slow but complete");
    assert!(r.text.trim_end().ends_with("data: [DONE]"));
}

#[test]
fn json_logs_on_request() {
    let rig = Rig::with(MIDIR_TOML, &[("MIDIR_LOG_FORMAT", "json")]);
    rig.upstream.add("ok");
    rig.http.post("/v1/chat/completions", &json!({"model": "gpt-5.1", "messages": user("hi")}));
    let log = rig.server.logs();
    let lines: Vec<&str> = log.lines().filter(|l| l.starts_with('{')).collect();
    assert!(lines.len() >= 3, "{log}");
    for l in lines {
        let v: serde_json::Value = serde_json::from_str(l).unwrap();
        assert!(v["fields"]["message"].is_string() && v["level"].is_string(), "{l}");
    }
}

#[test]
fn the_key_may_come_in_either_header_and_healthcheck_reads_string_ports() {
    let rig = Rig::with(MIDIR_TOML, &[("MIDIR_API_KEY", "s3cret")]);
    rig.upstream.add("ok");
    let body = json!({"model": "gpt-5.1", "max_tokens": 9, "messages": user("hi")});
    // an SDK configured with both a token and a key sends both
    let r = rig.http.post_with_headers("/v1/messages", &body, &[("authorization", "Bearer other"), ("x-api-key", "s3cret")]);
    assert_eq!(r.status, 200, "{}", r.text);
    let port = rig.server.port.to_string();
    let toml = "[server]\nport = \"${HC_PORT}\"\n";
    assert!(run(&["--healthcheck"], Some(toml), &[("HC_PORT", port.as_str())]).status.success());
}
