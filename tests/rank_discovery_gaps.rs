#![cfg(unix)]
//! Issue #4 through the actual CLI: partial discovery must not become an
//! empty roster when supported names can still be resolved safely. These
//! checks are offline and have no credentials or persisted ranking results.

use serde_json::{Value, json};
use skillranker::identity::ContentHash;
use skillranker::limits::{DISCOVERY_FILES, DISCOVERY_PARSED_BYTES};
use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, symlink};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!(
                "sr-rank-discovery-gap-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        // Retained for inspection; do not remove the fixture tree on drop.
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        for directory in ["workspace", "home/.claude/skills", "config/sr", "outside"] {
            fs::create_dir_all(root.join(directory)).unwrap();
        }
        let fixture = Self { root };
        let context = json!({
            "schema_version": 1,
            "harness": "claude_code",
            "producer_id": "discovery-gap-regression",
            "workspace_root": fixture.root.join("workspace").to_string_lossy(),
            "session_id": "discovery-gap-session",
            "agent_id": null,
            "branch_id": null,
            "context_epoch": null,
            "current_request": {
                "event_id": "request-1",
                "text": "Which skill helps fix a failing Rust test?",
                "attachments_omitted": false,
                "essential_attachment_missing": false
            },
            "events": [],
            "explicit_skill_references": [],
            "supplied_loads": []
        });
        fs::write(
            fixture.root.join("workspace/context.json"),
            serde_json::to_vec(&context).unwrap(),
        )
        .unwrap();
        fixture
    }

    fn skill(&self, base: &str, name: &str, extra: &str) {
        let directory = self.root.join(base).join(".claude/skills").join(name);
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("SKILL.md"),
            format!(
                "---\nname: {name}\ndescription: Helps repair failing Rust tests.\n{extra}---\nBody.\n"
            ),
        )
        .unwrap();
    }

    fn run(&self, args: &[&str]) -> (Output, Value) {
        let output = Command::new(env!("CARGO_BIN_EXE_sr"))
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .current_dir(self.root.join("workspace"))
            .args(args)
            .output()
            .unwrap();
        let value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "invalid JSON: {error}; stdout: {}; stderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (output, value)
    }

    fn rank(&self, extra: &[&str]) -> (Output, Value) {
        let mut args = vec![
            "rank",
            "--context",
            "context.json",
            "--offline",
            "--no-persist",
            "--json",
            "--timeout-ms",
            "10000",
        ];
        args.extend_from_slice(extra);
        self.run(&args)
    }

    fn assert_cache_miss(&self, eligible: usize) -> Value {
        let (output, value) = self.rank(&[]);
        assert_eq!(output.status.code(), Some(11), "{value}");
        assert_eq!(value["error"]["kind"], "cache-miss", "{value}");
        assert_eq!(value["decision"], "unavailable");
        assert_eq!(value["roster"]["eligible"], eligible, "{value}");
        assert_eq!(value["roster"]["partial"], true, "{value}");
        assert_eq!(value["usage"]["requests"], 0);
        assert_eq!(value["usage"]["http_attempts"], 0);
        assert!(value["skills"].as_array().unwrap().is_empty());
        assert!(!value.to_string().contains(self.root.to_str().unwrap()));
        value
    }

    fn listing(&self) -> Value {
        let (output, value) = self.run(&["roster", "--json"]);
        assert_eq!(output.status.code(), Some(0), "{value}");
        assert!(output.stderr.is_empty());
        assert_eq!(value["partial"], true);
        value
    }

    fn assert_explicit(&self, name: &str) {
        let (output, value) = self.rank(&["--require-skill", name]);
        assert_eq!(output.status.code(), Some(0), "{value}");
        assert_eq!(value["decision"], "explicit");
        assert_eq!(value["skills"][0]["invocation_name"], name);
        assert_eq!(value["usage"]["requests"], 0);
        assert_eq!(value["usage"]["http_attempts"], 0);
    }
}

fn ids(listing: &Value) -> BTreeSet<String> {
    listing["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|record| record["skill_id"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn one_unrelated_directory_link_never_turns_three_skills_into_empty_roster() {
    let fixture = Fixture::new();
    for name in ["alpha", "beta", "gamma"] {
        fixture.skill("home", name, "");
    }
    fixture.assert_cache_miss(3);
    let original = fixture.listing();
    let original_ids = ids(&original);
    assert_eq!(original_ids.len(), 3);
    let cause = "symlinked-directory-skipped";
    assert!(original["source_causes"].get(cause).is_none());

    let link = fixture
        .root
        .join("home/.claude/skills/private-symlink-canary");
    fs::write(
        fixture.root.join("outside/SKILL.md"),
        "# Must not be admitted\n\nprivate-skill-body-canary\n",
    )
    .unwrap();
    symlink(fixture.root.join("outside"), &link).unwrap();
    let ranked = fixture.assert_cache_miss(3);
    assert!(!ranked.to_string().contains("private-symlink-canary"));
    assert!(!ranked.to_string().contains("private-skill-body-canary"));
    fixture.assert_explicit("alpha");
    let partial = fixture.listing();
    assert_eq!(partial["source_causes"][cause], 1);
    assert_eq!(partial["counts"]["skills"], 3);
    assert_eq!(ids(&partial), original_ids);
    assert!(!partial.to_string().contains("private-symlink-canary"));

    // The linked skill remains excluded even when explicitly requested.
    let (output, value) = fixture.rank(&["--require-skill", "private-symlink-canary"]);
    assert_ne!(output.status.code(), Some(0), "{value}");
    assert_ne!(value["decision"], "explicit");

    // Reverse the only discovery change without deleting the retained fixture.
    fs::rename(&link, fixture.root.join("retained-link")).unwrap();
    fixture.assert_cache_miss(3);
    let restored = fixture.listing();
    assert!(restored["source_causes"].get(cause).is_none());
    assert_eq!(ids(&restored), original_ids);
}

#[test]
fn real_walk_ceiling_retains_clean_names_and_the_actual_personal_winner() {
    let fixture = Fixture::new();
    for name in ["alpha", "beta", "gamma"] {
        fixture.skill("workspace", name, "");
    }
    let references = fixture
        .root
        .join("workspace/.claude/skills/alpha/references");
    fs::create_dir_all(&references).unwrap();
    // The breadth-first walk must visit all three direct SKILL.md files before
    // reaching these support files, independent of filesystem entry order.
    for index in 0..=DISCOVERY_FILES.max() {
        fs::write(references.join(format!("note-{index:05}.txt")), "note").unwrap();
    }
    fixture.assert_cache_miss(3);
    let partial = fixture.listing();
    assert_eq!(partial["counts"]["skills"], 3);
    assert!(partial["source_causes"]["entry-limit"].as_u64().unwrap() > 0);
    fixture.assert_explicit("alpha");

    // Personal skill directories are visited before the project's nested
    // support files. Beta remains manual-only, and delta is no longer missed.
    fixture.skill("home", "beta", "disable-model-invocation: true\n");
    fixture.skill("home", "delta", "");
    fixture.assert_cache_miss(3); // alpha, gamma, delta; never project beta.
    fixture.assert_explicit("gamma");
    fixture.assert_explicit("delta");
    let (output, value) = fixture.rank(&["--require-skill", "beta"]);
    assert_eq!(output.status.code(), Some(0), "{value}");
    assert_eq!(value["decision"], "explicit");
    assert_eq!(value["skills"][0]["invocation_name"], "beta");
    let personal = fs::read(fixture.root.join("home/.claude/skills/beta/SKILL.md")).unwrap();
    assert_eq!(
        value["skills"][0]["content_hash"],
        ContentHash::from_bytes(&personal).as_str()
    );
    assert_eq!(value["usage"]["http_attempts"], 0);
}

#[test]
fn a_rejected_oversized_personal_competitor_still_blocks_project_fallback() {
    let fixture = Fixture::new();
    fixture.skill("workspace", "alpha", "");
    fixture.skill("workspace", "beta", "");
    fixture.skill("home", "beta", "disable-model-invocation: true\n");
    fs::OpenOptions::new()
        .write(true)
        .open(fixture.root.join("home/.claude/skills/beta/SKILL.md"))
        .unwrap()
        .set_len(DISCOVERY_PARSED_BYTES.max() as u64 + 1)
        .unwrap();
    fixture.assert_cache_miss(1);
    fixture.assert_explicit("alpha");
    let partial = fixture.listing();
    assert_eq!(partial["record_causes"]["oversized"], 1);
    assert!(partial["source_causes"].get("byte-limit").is_none());
    let (output, value) = fixture.rank(&["--require-skill", "beta"]);
    assert_ne!(output.status.code(), Some(0), "{value}");
    assert_ne!(value["decision"], "explicit");
}

#[test]
fn an_oversized_project_skill_does_not_empty_the_personal_roster() {
    let fixture = Fixture::new();
    fixture.skill("workspace", "oversized-private-name", "");
    fs::OpenOptions::new()
        .write(true)
        .open(
            fixture
                .root
                .join("workspace/.claude/skills/oversized-private-name/SKILL.md"),
        )
        .unwrap()
        .set_len(DISCOVERY_PARSED_BYTES.max() as u64 + 1)
        .unwrap();
    for name in ["alpha", "beta", "gamma"] {
        fixture.skill("home", name, "");
    }
    let value = fixture.assert_cache_miss(3);
    assert!(!value.to_string().contains("oversized-private-name"));
    let listing = fixture.listing();
    assert_eq!(listing["counts"]["skills"], 3);
    assert_eq!(listing["record_causes"]["oversized"], 1);
    assert!(listing["source_causes"].get("byte-limit").is_none());
    assert!(!listing.to_string().contains("oversized-private-name"));
    fixture.assert_explicit("beta");
}

#[test]
fn a_fifo_personal_override_is_not_silently_treated_as_absent() {
    let fixture = Fixture::new();
    fixture.skill("workspace", "alpha", "");
    fixture.skill("workspace", "beta", "");
    let path = fixture.root.join("home/.claude/skills/beta/SKILL.md");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    nix::unistd::mkfifo(&path, nix::sys::stat::Mode::S_IRWXU).unwrap();
    fixture.assert_cache_miss(1);
    fixture.assert_explicit("alpha");
    let listing = fixture.listing();
    assert_eq!(listing["record_causes"]["unreadable"], 1);
    let (output, value) = fixture.rank(&["--require-skill", "beta"]);
    assert_ne!(output.status.code(), Some(0), "{value}");
    assert_ne!(value["decision"], "explicit");
}
