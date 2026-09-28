#![cfg(unix)]
//! What the real `sr` binary says about its own persistent store (sr-un4j).
//!
//! The existing cache tests construct a `CacheStore` directly, so none of them
//! exercised the store the CLI builds for itself. That gap let an unavailable
//! cache stay completely silent: a shared-writable ancestor is refused, which
//! is correct, but the run disclosed nothing and reported `unavailable`
//! persistence even when the cache was serving hits. These cases pin the three
//! reported states to reality. Every case runs offline: no provider is
//! contacted, and the store is opened before any provider admission.
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

// Intentionally retained: repository policy forbids automatic tree deletion.
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    /// A workspace, one personal skill and one transcript this workspace can
    /// claim, so discovery yields the session identity a store needs.
    fn new(root_mode: u32) -> Self {
        let root = std::env::temp_dir().join(format!(
            "sr-cache-reuse-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let f = Self { root };
        fs::create_dir_all(f.workspace()).unwrap();
        fs::create_dir_all(f.home().join(".config")).unwrap();
        let skill = f.home().join(".claude/skills/rust-tester");
        fs::create_dir_all(&skill).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            "---\ndescription: Runs and repairs failing rust tests.\n---\nBody.\n",
        )
        .unwrap();
        let session = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let encoded: String = f
            .workspace()
            .to_string_lossy()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let transcripts = f.home().join(".claude/projects").join(encoded);
        fs::create_dir_all(&transcripts).unwrap();
        fs::write(
            transcripts.join(format!("{session}.jsonl")),
            json!({"type":"user","uuid":"u1","parentUuid":null,"sessionId":session,
                   "cwd":f.workspace().to_string_lossy(),
                   "timestamp":"2026-09-20T03:00:00.000Z",
                   "message":{"role":"user","content":"Fix the failing rust test."}})
            .to_string()
                + "\n",
        )
        .unwrap();
        // Owner-only everywhere the store walks, then the caller's chosen mode
        // on the fixture root itself.
        for dir in [f.home(), f.workspace()] {
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::set_permissions(&f.root, fs::Permissions::from_mode(root_mode)).unwrap();
        f
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }
    fn workspace(&self) -> PathBuf {
        self.root.join("workspace")
    }

    /// Ranks offline, so the run reaches the store and stops at provider
    /// admission without contacting anything.
    fn rank(&self, extra: &[&str]) -> Value {
        let output = Command::new(env!("CARGO_BIN_EXE_sr"))
            .env_clear()
            .env("HOME", self.home())
            .env("XDG_CONFIG_HOME", self.home().join(".config"))
            // These cases test persistence, not the deadline. A debug binary on
            // a degraded build worker overran the 3 s production default and
            // reported `timeout` instead of its store's state (sr-0sgw).
            .env("SR_TIMEOUT_MS", "10000")
            .current_dir(self.workspace())
            .args(["rank", "--offline", "--json"])
            .args(extra)
            .output()
            .unwrap();
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

fn warning_kinds(value: &Value) -> Vec<String> {
    value["warnings"]
        .as_array()
        .map(|warnings| {
            warnings
                .iter()
                .filter_map(|w| w["kind"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn cache_dir(home: &Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    return home.join("Library/Caches/sr");
    #[cfg(not(target_os = "macos"))]
    home.join(".cache/sr")
}

/// Whether a store may live under `path`, by the same rule the store applies to
/// every ancestor: owned by this user or by root, and not group/other writable
/// unless it is root-owned and sticky (`/tmp`). Test machines differ — a shared
/// build worker's temp tree is often group writable — so the expected reported
/// state is derived rather than assumed. Without this the positive case passes
/// on a workstation and fails on a build worker for an environmental reason.
fn store_may_live_under(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let me = nix::unistd::getuid().as_raw();
    let mut current = Some(path);
    while let Some(dir) = current {
        let Ok(metadata) = fs::metadata(dir) else {
            return false;
        };
        let mode = metadata.mode() & 0o7777;
        let uid = metadata.uid();
        let root_sticky = uid == 0 && mode & 0o1000 != 0;
        if (uid != 0 && uid != me) || (mode & 0o022 != 0 && !root_sticky) {
            return false;
        }
        current = dir.parent();
    }
    true
}

#[test]
fn a_qualified_store_reports_recorded_persistence() {
    let f = Fixture::new(0o700);
    let value = f.rank(&[]);
    // Discovery found the session, so a store was scoped and attempted.
    assert!(
        warning_kinds(&value).contains(&"discovered-session".to_owned()),
        "{value}"
    );
    let warned = warning_kinds(&value).contains(&"cache-unavailable".to_owned());
    let database = cache_dir(&f.home()).join("cache.sqlite3").exists();
    if store_may_live_under(&f.home()) {
        assert_eq!(value["persistence"], "recorded", "{value}");
        assert!(!warned, "a usable cache warns about nothing: {value}");
        assert!(
            database,
            "the CLI builds its own store, not just the tests: {value}"
        );
    } else {
        // This machine cannot host a private store at all, so the only correct
        // report is an unavailable one that says so.
        assert_eq!(value["persistence"], "unavailable", "{value}");
        assert!(warned, "an uncached run must say so: {value}");
        assert!(!database, "{value}");
    }
}

#[test]
fn a_store_refused_for_a_shared_writable_ancestor_is_disclosed() {
    // Group-writable ancestors can rename the store's directory, so the store
    // refuses them. That refusal is correct; staying silent about it is not.
    let f = Fixture::new(0o775);
    let value = f.rank(&[]);
    assert_eq!(value["persistence"], "unavailable", "{value}");
    assert!(
        warning_kinds(&value).contains(&"cache-unavailable".to_owned()),
        "an uncached run must say so: {value}"
    );
    assert!(
        !cache_dir(&f.home()).join("cache.sqlite3").exists(),
        "no store is created under a shared-writable ancestor"
    );
    // The disclosure carries no path, mode or errno. A path is long enough that its
    // absence can be checked over the whole document, but a mode is three digits:
    // scanning everything for "775" also scans content-addressed digests, and a hex
    // digest containing those digits is not a leaked mode. So the mode and errno are
    // checked against the text a reader actually sees.
    let text = serde_json::to_string(&value).unwrap();
    assert!(!text.contains(f.root.to_str().unwrap()), "{value}");
    let mut prose = String::new();
    for warning in value["warnings"].as_array().into_iter().flatten() {
        prose.push_str(warning["message"].as_str().unwrap_or_default());
        prose.push('\n');
    }
    for field in ["message", "hint"] {
        prose.push_str(value["error"][field].as_str().unwrap_or_default());
        prose.push('\n');
    }
    assert!(
        !prose.contains("775") && !prose.contains("EACCES"),
        "{value}"
    );
}

#[test]
fn a_relative_transcript_path_is_resolved_against_the_workspace() {
    // sr-gl5j: the identity lookup resolved a relative path while the read used
    // it raw, so a transcript named relatively was reported as "not a regular
    // file". Both now resolve against the workspace.
    let f = Fixture::new(0o700);
    let transcript = f.workspace().join("session.jsonl");
    fs::write(
        &transcript,
        json!({"type":"user","uuid":"u1","parentUuid":null,
               "message":{"role":"user","content":"Fix the failing rust test."}})
        .to_string()
            + "\n",
    )
    .unwrap();
    let relative = f.rank(&["--transcript", "session.jsonl", "--harness", "claude_code"]);
    let absolute = f.rank(&[
        "--transcript",
        transcript.to_str().unwrap(),
        "--harness",
        "claude_code",
    ]);
    // Offline ranking stops at provider admission, so both reach the same
    // typed cache-miss rather than a malformed-input refusal.
    assert_eq!(
        relative["error"]["kind"], absolute["error"]["kind"],
        "relative {relative} absolute {absolute}"
    );
    assert_ne!(relative["error"]["kind"], "malformed-input", "{relative}");
}

#[test]
fn a_cache_disabled_by_an_effect_flag_reports_disabled_not_unavailable() {
    let f = Fixture::new(0o700);
    let value = f.rank(&["--no-cache"]);
    assert_eq!(value["persistence"], "disabled", "{value}");
    assert!(
        !warning_kinds(&value).contains(&"cache-unavailable".to_owned()),
        "a cache turned off by choice is not a failure: {value}"
    );
}
