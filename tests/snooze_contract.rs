#![cfg(any(target_os = "linux", target_os = "macos"))]
//! Trusted, session-scoped advisory snoozes (sr-roadmap-l1i.7.8, I04),
//! through the real binary against the loopback TLS Jev fixture. Synthetic
//! skills and sessions only.
//!
//! Each consequential refusal keeps its positive twin: a snoozed skill leaves
//! only its own scope, muting everything skips Jev while an explicit request
//! still resolves, and a snooze landing while a ranking is pending withholds
//! that ranking but not one in a sibling session.

mod support;

use serde_json::{Value, json};
use std::io::{BufRead, BufReader};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Home {
    root: PathBuf,
}

impl Home {
    fn new() -> Self {
        let root = support::private_store_dir("snooze");
        for dir in ["home", "config", "cache", "data", "workspace", "scratch"] {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(root.join(dir))
                .unwrap();
        }
        for (name, description) in [
            ("alpha", "Runs and repairs failing rust tests."),
            ("beta", "Drafts release notes from git history."),
        ] {
            let dir = root.join("workspace/.claude/skills").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {description}\n---\nBody.\n"),
            )
            .unwrap();
        }
        std::fs::write(
            root.join("fixture-ca.pem"),
            include_bytes!("fixtures/jev-tls/ca.pem"),
        )
        .unwrap();
        let home = Self { root };
        home.json(1, &["ledger", "init", "--json"]);
        home
    }

    fn snooze_file(&self) -> PathBuf {
        self.root.join("config/sr/snoozes.toml")
    }

    fn command(&self, port: u16, config: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_sr"))
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", config)
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_STATE_HOME", self.root.join("home/state"))
            .env("TYPESAFE_API_KEY", "synthetic-snooze-canary")
            .env("TYPESAFE_ENDPOINT", format!("https://localhost:{port}"))
            .env("SSL_CERT_FILE", self.root.join("fixture-ca.pem"))
            .current_dir(self.root.join("workspace"))
            .args(args)
            .output()
            .unwrap()
    }

    fn run(&self, port: u16, args: &[&str]) -> Output {
        self.command(port, &self.root.join("config"), args)
    }

    fn json(&self, port: u16, args: &[&str]) -> Value {
        let out = self.run(port, args);
        assert!(out.status.success(), "{args:?}: {}", describe(&out));
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn context(&self, session: Option<&str>, turn: &str) -> String {
        let path = self.root.join("workspace").join(format!(
            "context-{}.json",
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let value = json!({
            "schema_version": 1, "harness": "claude_code", "producer_id": "snooze-contract",
            "workspace_root": self.root.join("workspace").to_str().unwrap(),
            "session_id": session, "agent_id": null, "branch_id": null, "context_epoch": null,
            "current_request": {"event_id": format!("{}-{turn}", session.unwrap_or("none")),
                "text": "Our rust test suite started failing; find out why.",
                "attachments_omitted": false, "essential_attachment_missing": false},
            "events": [], "explicit_skill_references": [], "supplied_loads": []
        });
        std::fs::write(&path, value.to_string()).unwrap();
        path.to_string_lossy().into_owned()
    }

    /// Ranks one turn; returns the exit, the document and the provider's
    /// served requests.
    fn rank(&self, provider: Provider, context: &str, extra: &[&str]) -> (Output, Value, usize) {
        let mut args = vec![
            "rank",
            "--context",
            context,
            "--allow-network",
            "--timeout-ms",
            "12000",
            "--json",
        ];
        args.extend_from_slice(extra);
        let out = self.run(provider.port, &args);
        let served = provider.finish();
        let doc = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
        (out, doc, served)
    }

    /// A recorded ranking in `session`: its event and top suggested skill.
    fn recorded(&self, session: &str, turn: &str) -> (String, String) {
        let context = self.context(Some(session), turn);
        let (out, doc, served) = self.rank(Provider::start(self, "useful", &[]), &context, &[]);
        assert!(out.status.success(), "{}", describe(&out));
        assert_eq!(served, 2);
        assert_eq!(doc["decision"], "ranked", "{doc}");
        (
            doc["event_id"].as_str().unwrap().to_owned(),
            doc["skills"][0]["skill_id"].as_str().unwrap().to_owned(),
        )
    }
}

fn describe(out: &Output) -> String {
    format!(
        "exit {:?}\n{}\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn suggested(doc: &Value) -> Vec<String> {
    doc["skills"]
        .as_array()
        .unwrap()
        .iter()
        .map(|skill| skill["skill_id"].as_str().unwrap().to_owned())
        .collect()
}

fn error_kind(out: &Output) -> (Option<i32>, String) {
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    let kind = doc["error"]["kind"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| String::from_utf8_lossy(&out.stderr).into_owned());
    (out.status.code(), kind)
}

/// The loopback TLS Jev fixture; `args` are its optional mutation target and text.
struct Provider {
    child: Child,
    lines: BufReader<ChildStdout>,
    port: u16,
}

impl Provider {
    fn start(home: &Home, scenario: &str, args: &[&str]) -> Self {
        let dir = home
            .root
            .join(format!("provider-{}", NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir(&dir).unwrap();
        for (name, bytes) in [
            (
                "provider_server.py",
                &include_bytes!("fixtures/jev-tls/provider_server.py")[..],
            ),
            (
                "server.pem",
                &include_bytes!("fixtures/jev-tls/server.pem")[..],
            ),
            (
                "server.key",
                &include_bytes!("fixtures/jev-tls/server.key")[..],
            ),
        ] {
            std::fs::write(dir.join(name), bytes).unwrap();
        }
        let mut child = Command::new("/usr/bin/python3")
            .arg(dir.join("provider_server.py"))
            .arg(scenario)
            .args(args)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap());
        let mut hello = String::new();
        lines.read_line(&mut hello).unwrap();
        let hello: Value = serde_json::from_str(&hello).unwrap();
        let port = u16::try_from(hello["port"].as_u64().unwrap()).unwrap();
        Self { child, lines, port }
    }

    /// Requests the provider answered.
    fn finish(mut self) -> usize {
        let mut done = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        std::io::Write::write_all(&mut done, b"DONE").unwrap();
        drop(done);
        let mut served = 0;
        loop {
            let mut line = String::new();
            assert!(
                self.lines.read_line(&mut line).unwrap() > 0,
                "provider ended early"
            );
            let value: Value = serde_json::from_str(&line).unwrap();
            if value["done"] == true {
                break;
            }
            served += usize::from(value["handshake_rejected"] != true);
        }
        let _ = self.child.wait();
        served
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn a_skill_snooze_withholds_only_that_skill_in_its_own_session() {
    let home = Home::new();
    let (event, top) = home.recorded("s1", "t1");
    let stats_before = home.json(1, &["stats", "--json"]);

    // Preview writes nothing and says so.
    let preview = home.json(
        1,
        &["snooze", &event, "--skill", &top, "--for", "30m", "--json"],
    );
    assert_eq!(preview["applied"], false, "{preview}");
    assert_eq!(preview["action"], "skill");
    assert_eq!(preview["skill_id"], top.as_str());
    assert!(
        preview["scope"]["session_id"]
            .as_str()
            .unwrap()
            .starts_with("ranking-session-v2-")
    );
    assert_eq!(preview["scope"]["agent_branch"], "main");
    assert_eq!(preview["duration_ms"], 1_800_000);
    assert!(preview["effect"].as_str().unwrap().contains("Preview only"));
    assert!(
        preview["effect"]
            .as_str()
            .unwrap()
            .contains("cannot be recalled")
    );
    assert!(!home.snooze_file().exists(), "a preview wrote the file");

    let applied = home.json(
        1,
        &[
            "snooze", &event, "--skill", &top, "--for", "30m", "--apply", "--json",
        ],
    );
    assert_eq!(applied["applied"], true, "{applied}");
    assert_eq!(applied["entries_after"], 1);
    let mode = std::fs::metadata(home.snooze_file())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "owner-only trusted configuration");
    // Echoing the event ID is not an exposure of its advice.
    let stats_snoozed = home.json(1, &["stats", "--json"]);
    assert_eq!(
        stats_snoozed["turns"]["emitted_suggestions"], stats_before["turns"]["emitted_suggestions"],
        "a snooze command marked a ranking as emitted"
    );

    // Same session: the snoozed skill leaves advisory selection before the
    // wide request, and stays visible in the explanation.
    let context = home.context(Some("s1"), "t2");
    let (out, doc, served) = home.rank(
        Provider::start(&home, "useful", &[]),
        &context,
        &["--explain", "--why-not", &top],
    );
    assert!(out.status.success(), "{}", describe(&out));
    assert_eq!(served, 2);
    assert_eq!(doc["decision"], "ranked", "{doc}");
    assert!(!suggested(&doc).contains(&top), "{doc}");
    assert_eq!(doc["roster"]["eligible"], 1, "{doc}");
    assert_eq!(
        doc["roster"]["provenance"]["snoozes"],
        json!({"all": false, "skill_ids": [top.as_str()], "uncertain_expiry": 0}),
        "{doc}"
    );
    let policy = doc["trace"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["skill_id"] == top.as_str() && e["stage"] == "local-policy")
        .cloned();
    assert_eq!(
        policy.as_ref().map(|e| (&e["reason"], &e["hint"])),
        Some((&json!("snoozed"), &json!("review-snoozes"))),
        "{doc}"
    );

    // A sibling session is untouched: the skill is suggested there.
    let sibling = home.context(Some("s2"), "t1");
    let (out, doc, _) = home.rank(Provider::start(&home, "useful", &[]), &sibling, &[]);
    assert!(out.status.success(), "{}", describe(&out));
    assert_eq!(suggested(&doc).first(), Some(&top), "{doc}");
    assert!(
        doc["roster"]["provenance"].get("snoozes").is_none(),
        "{doc}"
    );

    // A snooze is not a usefulness label: judgments are unchanged.
    let stats_after = home.json(1, &["stats", "--json"]);
    assert_eq!(stats_after["judgments"], stats_before["judgments"]);
}

#[test]
fn muting_everything_skips_jev_while_explicit_requests_still_resolve() {
    let home = Home::new();
    let (event, _) = home.recorded("s1", "t1");
    home.json(
        1,
        &[
            "snooze", &event, "--all", "--for", "2h", "--apply", "--json",
        ],
    );

    // No provider request, with or without persistence: snoozes are read
    // like configuration.
    for extra in [&[][..], &["--no-persist"][..]] {
        let context = home.context(Some("s1"), &format!("all{}", extra.len()));
        let (out, doc, served) = home.rank(Provider::start(&home, "useful", &[]), &context, extra);
        assert!(out.status.success(), "{extra:?}: {}", describe(&out));
        assert_eq!(served, 0, "{extra:?}: Jev was called");
        assert_eq!(doc["decision"], "abstain", "{doc}");
        assert_eq!(doc["reason"], "snoozed", "{doc}");
        assert_eq!(doc["roster"]["provenance"]["snoozes"]["all"], true);
    }

    // An explicit requirement ignores the advisory snooze.
    let context = home.context(Some("s1"), "explicit");
    let (out, doc, served) = home.rank(
        Provider::start(&home, "useful", &[]),
        &context,
        &["--require-skill", "beta"],
    );
    assert!(out.status.success(), "{}", describe(&out));
    assert_eq!(served, 0);
    assert_eq!(doc["decision"], "explicit", "{doc}");

    // Clearing restores advice from the next ranking.
    let preview = home.json(1, &["snooze", &event, "--clear", "--json"]);
    assert_eq!(
        (preview["applied"].clone(), preview["clears"].clone()),
        (json!(false), json!(1))
    );
    let cleared = home.json(1, &["snooze", &event, "--clear", "--apply", "--json"]);
    assert_eq!(cleared["clears"], 1, "{cleared}");
    assert_eq!(cleared["entries_after"], 0);
    let context = home.context(Some("s1"), "after-clear");
    let (_, doc, served) = home.rank(Provider::start(&home, "useful", &[]), &context, &[]);
    assert_eq!(served, 2);
    assert_eq!(doc["decision"], "ranked", "{doc}");
}

#[test]
fn a_snooze_applied_while_a_ranking_is_pending_withholds_only_its_scope() {
    let home = Home::new();
    let (event_s1, top) = home.recorded("s1", "t1");
    let (event_s2, _) = home.recorded("s2", "t1");
    std::fs::create_dir_all(home.snooze_file().parent().unwrap()).unwrap();

    // `sr snooze --apply` renders each control in a scratch configuration
    // root; the provider then writes those exact bytes into the live root
    // while it answers the rerank request, after admission and before the
    // ranking publishes.
    let render = |event: &str, label: &str| {
        let scratch = home.root.join("scratch").join(label);
        let out = home.command(
            1,
            &scratch,
            &[
                "snooze", event, "--skill", &top, "--for", "30m", "--apply", "--json",
            ],
        );
        assert!(out.status.success(), "{}", describe(&out));
        std::fs::read_to_string(scratch.join("sr/snoozes.toml")).unwrap()
    };
    let for_s1 = render(&event_s1, "s1");
    let for_s2 = render(&event_s2, "s2");
    let target = home.snooze_file();
    let target = target.to_str().unwrap();

    // Positive twin: a sibling session's new snooze leaves this ranking valid.
    let context = home.context(Some("s1"), "sibling-race");
    let (out, doc, served) = home.rank(
        Provider::start(&home, "useful+write-on-rerank", &[target, &for_s2]),
        &context,
        &[],
    );
    assert!(out.status.success(), "{}", describe(&out));
    assert_eq!(served, 2);
    assert_eq!(suggested(&doc).first(), Some(&top), "{doc}");

    // This session's new snooze: the stale advice is withheld. The requests
    // were already admitted, and their cost is not undone.
    std::fs::write(home.snooze_file(), "schema_version = 1\n").unwrap();
    let context = home.context(Some("s1"), "own-race");
    let (out, _, served) = home.rank(
        Provider::start(&home, "useful+write-on-rerank", &[target, &for_s1]),
        &context,
        &[],
    );
    assert_eq!(
        served, 2,
        "both requests were sent before the snooze landed"
    );
    assert_eq!(error_kind(&out), (Some(3), "superseded".to_owned()));
}

#[test]
fn an_uncertain_expiry_stays_muted_and_doctor_shows_it() {
    let home = Home::new();
    let (event, top) = home.recorded("s1", "t1");
    let preview = home.json(
        1,
        &["snooze", &event, "--skill", &top, "--for", "30m", "--json"],
    );
    let session = preview["scope"]["session_id"].as_str().unwrap();
    // Created an hour in the future: the clock moved back after writing it.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let workspace = home.root.join("workspace");
    std::fs::create_dir_all(home.snooze_file().parent().unwrap()).unwrap();
    std::fs::write(
        home.snooze_file(),
        format!(
            "schema_version = 1\n\n[[snooze]]\nevent_id = \"{event}\"\nworkspace_root = \"{}\"\n\
             session_id = \"{session}\"\nagent_branch = \"main\"\nskill_id = \"{top}\"\n\
             created_at_unix_ms = {}\nexpires_at_unix_ms = {}\n",
            workspace.to_str().unwrap(),
            now + 3_600_000,
            now + 5_400_000,
        ),
    )
    .unwrap();
    let context = home.context(Some("s1"), "t2");
    let (out, doc, _) = home.rank(Provider::start(&home, "useful", &[]), &context, &[]);
    assert!(out.status.success(), "{}", describe(&out));
    assert!(!suggested(&doc).contains(&top), "{doc}");
    assert_eq!(
        doc["roster"]["provenance"]["snoozes"]["uncertain_expiry"],
        1
    );

    let doctor = home.json(1, &["doctor", "--json"]);
    let snoozes = &doctor["checks"]["hook"]["snoozes"];
    assert_eq!(snoozes["state"], "active", "{doctor}");
    assert_eq!(snoozes["uncertain_expiry"], 1);
    assert_eq!(snoozes["muting"][0]["status"], "uncertain-expiry");
    assert!(snoozes["next_step"].is_string());
    // Reading never cleans up: the file is byte-identical after ranking.
    assert!(
        std::fs::read_to_string(home.snooze_file())
            .unwrap()
            .contains(&format!("created_at_unix_ms = {}", now + 3_600_000))
    );
}

#[test]
fn a_malformed_snooze_file_fails_ranking_closed_and_doctor_explains_it() {
    let home = Home::new();
    std::fs::create_dir_all(home.snooze_file().parent().unwrap()).unwrap();
    std::fs::write(home.snooze_file(), "schema_version = 1\ncolor = \"red\"\n").unwrap();
    let context = home.context(Some("s1"), "t1");
    let (out, _, served) = home.rank(Provider::start(&home, "useful", &[]), &context, &[]);
    assert_eq!(served, 0);
    assert_eq!(
        error_kind(&out),
        (Some(2), "invalid-configuration".to_owned())
    );
    let doctor = home.json(1, &["doctor", "--json"]);
    assert_eq!(doctor["checks"]["hook"]["snoozes"]["state"], "invalid");
}

#[test]
fn unattributed_events_and_incompatible_modes_are_refused() {
    let home = Home::new();
    let (event, top) = home.recorded("s1", "t1");
    for (args, code) in [
        (vec!["--skill", top.as_str(), "--all", "--for", "30m"], 2),
        (vec!["--clear", "--for", "30m"], 2),
        (vec!["--all"], 2),
        (vec!["--skill", top.as_str()], 2),
        (vec!["--all", "--for", "25h"], 2),
        (vec!["--all", "--for", "30s"], 2),
        (vec![], 2),
    ] {
        let mut full = vec!["snooze", event.as_str()];
        full.extend(args.iter().copied());
        full.push("--apply");
        let out = home.run(1, &full);
        assert_eq!(
            out.status.code(),
            Some(code),
            "{full:?}: {}",
            describe(&out)
        );
    }
    let out = home.run(1, &["snooze", "no-such-event", "--all", "--for", "30m"]);
    assert_eq!(out.status.code(), Some(2), "{}", describe(&out));
    assert!(
        !home.snooze_file().exists(),
        "a refused change wrote the file"
    );

    // A ranking without a session identity records a placeholder session; no
    // snooze may be scoped to it.
    let context = home.context(None, "t1");
    let (out, doc, _) = home.rank(Provider::start(&home, "useful", &[]), &context, &[]);
    assert!(out.status.success(), "{}", describe(&out));
    let unattributed = doc["event_id"].as_str().unwrap();
    let out = home.run(
        1,
        &["snooze", unattributed, "--all", "--for", "30m", "--apply"],
    );
    assert_eq!(out.status.code(), Some(3), "{}", describe(&out));
    assert!(!home.snooze_file().exists());
}
