#![cfg(unix)]
//! P6 Claude hook installer and rollback contract.
//!
//! Satisfies contract requirements for bead `sr-roadmap-l1i.7.3`:
//! - Preview and apply only the managed Claude settings entry.
//! - Uses trusted absolute binary path, quoted arguments, UserPromptSubmit, and explicit timeout.
//! - Preserves unrelated settings, other hooks, and file permissions.
//! - Owner-only backups in private state directory before publication.
//! - Per-target locking to serialize cooperating installers.
//! - Base digest checking to abort on detected external changes.
//! - Idempotent installation.
//! - Refuses malformed settings without overwriting them.
//! - Reports enterprise / managed restrictions without bypassing.
//! - Displays conflicts for modified entries rather than broad removal.
//! - Clamps internal deadlines exceeding the installed outer budget.

use serde_json::json;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static FIXTURE_COUNTER: AtomicU64 = AtomicU64::new(0);

struct InstallerFixture {
    root: PathBuf,
}

impl InstallerFixture {
    fn new() -> Self {
        let root = PathBuf::from("/tmp").join(format!(
            "sr-installer-contract-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new()
            .recursive(true)
            .create(&root)
            .expect("create root");

        let f = Self { root };
        fs::create_dir_all(f.home()).unwrap();
        fs::create_dir_all(f.claude_dir()).unwrap();
        fs::create_dir_all(f.config_dir()).unwrap();
        fs::create_dir_all(f.workspace()).unwrap();

        for dir in [
            &f.root,
            &f.home(),
            &f.claude_dir(),
            &f.config_dir(),
            &f.workspace(),
        ] {
            let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
        }

        f
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn claude_dir(&self) -> PathBuf {
        self.home().join(".claude")
    }

    fn settings_path(&self) -> PathBuf {
        self.claude_dir().join("settings.json")
    }

    fn config_dir(&self) -> PathBuf {
        self.home().join(".config/sr")
    }

    fn workspace(&self) -> PathBuf {
        self.root.join("workspace")
    }

    fn state_dir(&self) -> PathBuf {
        self.home().join(".local/state/sr/backups")
    }

    fn run_sr(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_sr"))
            .env_clear()
            .env("HOME", self.home())
            .env("XDG_CONFIG_HOME", self.home().join(".config"))
            .env("XDG_STATE_HOME", self.home().join(".local/state"))
            .current_dir(self.workspace())
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("exec sr")
    }
}

#[test]
fn preview_install_by_default_does_not_create_file() {
    let fixture = InstallerFixture::new();
    assert!(!fixture.settings_path().exists());

    let out = fixture.run_sr(&["install-hook", "claude"]);
    assert_eq!(out.status.code(), Some(0));

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("---"), "must contain diff headers");
    assert!(stdout.contains("+++"), "must contain diff headers");
    assert!(stdout.contains("@@ hooks.UserPromptSubmit @@"));
    assert!(stdout.contains("hook claude"));
    assert!(stdout.contains("--apply to merge"));

    // settings.json must NOT have been created
    assert!(!fixture.settings_path().exists());
}

#[test]
fn apply_install_creates_settings_and_backup() {
    let fixture = InstallerFixture::new();

    let out = fixture.run_sr(&["install-hook", "claude", "--apply"]);
    assert_eq!(out.status.code(), Some(0));

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Successfully installed Claude hook"));
    assert!(stdout.contains("Backup created at"));
    assert!(stdout.contains("shadow"), "reports default shadow mode");

    // settings.json must exist and contain valid JSON
    assert!(fixture.settings_path().exists());
    let content = fs::read_to_string(fixture.settings_path()).unwrap();
    let val: serde_json::Value = serde_json::from_str(&content).unwrap();

    let hooks = &val["hooks"]["UserPromptSubmit"];
    assert!(hooks.is_array());
    let entry = &hooks[0];
    assert_eq!(entry["matcher"], "");
    assert_eq!(entry["hooks"][0]["type"], "command");
    assert!(entry["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .contains("hook claude"));
    // Four, not three: the harness timeout must STRICTLY exceed the 3,000 ms internal deadline,
    // because the internal clock starts after process startup and the last 200 ms of it are
    // reserved for writing the answer (sr-83cc). README documents four seconds.
    assert_eq!(entry["hooks"][0]["timeout"], 4);

    // Backup file must exist in state directory
    assert!(fixture.state_dir().exists());
    let backups: Vec<_> = fs::read_dir(fixture.state_dir())
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    assert_eq!(backups.len(), 1);
    let backup_meta = backups[0].metadata().unwrap();
    assert_eq!(backup_meta.permissions().mode() & 0o777, 0o600);
}

#[test]
fn install_refuses_timeout_not_above_internal_deadline() {
    let fixture = InstallerFixture::new();

    // Default internal deadline is 3000ms; outer timeout 3 equals it and
    // could kill sr during its output reserve. Refused with the minimum named.
    let out = fixture.run_sr(&["install-hook", "claude", "--timeout-secs", "3"]);
    assert_eq!(out.status.code(), Some(2));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("--timeout-secs 4"),
        "refusal names the required minimum: {stdout}"
    );
    assert!(!fixture.settings_path().exists());

    // A raised effective internal deadline raises the required outer timeout.
    let out = Command::new(env!("CARGO_BIN_EXE_sr"))
        .env_clear()
        .env("HOME", fixture.home())
        .env("XDG_CONFIG_HOME", fixture.home().join(".config"))
        .env("XDG_STATE_HOME", fixture.home().join(".local/state"))
        .env("SR_TIMEOUT_MS", "10000")
        .current_dir(fixture.workspace())
        .args(["install-hook", "claude", "--timeout-secs", "10"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("exec sr");
    assert_eq!(out.status.code(), Some(2));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("--timeout-secs 11"),
        "refusal scales with the effective deadline: {stdout}"
    );
    assert!(!fixture.settings_path().exists());

    // One second more covers the deadline plus the startup reserve.
    let out = Command::new(env!("CARGO_BIN_EXE_sr"))
        .env_clear()
        .env("HOME", fixture.home())
        .env("XDG_CONFIG_HOME", fixture.home().join(".config"))
        .env("XDG_STATE_HOME", fixture.home().join(".local/state"))
        .env("SR_TIMEOUT_MS", "10000")
        .current_dir(fixture.workspace())
        .args(["install-hook", "claude", "--timeout-secs", "11"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("exec sr");
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"timeout\": 11"),
        "previews the accepted timeout: {stdout}"
    );
}

#[test]
fn hook_clamp_budget_comes_from_the_installed_entry() {
    let fixture = InstallerFixture::new();

    // Install with an outer timeout of 10s: the internal budget becomes
    // 9000ms, so an 8000ms internal deadline is NOT clamped to 3000ms.
    let out = fixture.run_sr(&["install-hook", "claude", "--timeout-secs", "10", "--apply"]);
    assert_eq!(out.status.code(), Some(0));

    let hook_payload = json!({
        "hook_event_name": "UserPromptSubmit",
        "prompt": "test prompt",
        "prompt_id": "p-budget-1",
        "session_id": "session-budget-1",
        "cwd": fixture.workspace(),
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_sr"))
        .env_clear()
        .env("HOME", fixture.home())
        .env("XDG_CONFIG_HOME", fixture.home().join(".config"))
        .env("XDG_STATE_HOME", fixture.home().join(".local/state"))
        .current_dir(fixture.workspace())
        .arg("hook")
        .arg("claude")
        .arg("--timeout-ms")
        .arg("8000")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn sr hook claude");
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(&serde_json::to_vec(&hook_payload).unwrap());
    }
    let out = child.wait_with_output().expect("wait");
    assert_eq!(out.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("exceeds installed hook budget"),
        "8000ms fits the installed 9000ms budget, no clamp: {stderr}"
    );
}

#[test]
fn install_is_idempotent() {
    let fixture = InstallerFixture::new();

    // First install
    let out1 = fixture.run_sr(&["install-hook", "claude", "--apply"]);
    assert_eq!(out1.status.code(), Some(0));

    let content1 = fs::read_to_string(fixture.settings_path()).unwrap();

    // Second install
    let out2 = fixture.run_sr(&["install-hook", "claude", "--apply"]);
    assert_eq!(out2.status.code(), Some(0));

    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    assert!(stdout2.contains("already installed"));

    let content2 = fs::read_to_string(fixture.settings_path()).unwrap();
    assert_eq!(content1, content2, "settings.json must be identical");
}

#[test]
fn preserves_unrelated_settings_and_other_hooks() {
    let fixture = InstallerFixture::new();

    let initial_settings = json!({
        "theme": "dark",
        "secret_token": "super-secret-user-token-12345",
        "hooks": {
            "UserPromptSubmit": [
                {
                    "matcher": "git *",
                    "hooks": [
                        {
                            "type": "command",
                            "command": "check-git.sh",
                            "timeout": 10
                        }
                    ]
                }
            ],
            "PostToolUse": [
                {
                    "matcher": "",
                    "hooks": [
                        {
                            "type": "command",
                            "command": "linter.sh"
                        }
                    ]
                }
            ]
        }
    });

    fs::write(
        fixture.settings_path(),
        serde_json::to_string_pretty(&initial_settings).unwrap(),
    )
    .unwrap();

    // Preview: must not print unrelated secret-token
    let preview = fixture.run_sr(&["install-hook", "claude"]);
    assert_eq!(preview.status.code(), Some(0));
    let preview_stdout = String::from_utf8_lossy(&preview.stdout);
    assert!(!preview_stdout.contains("super-secret-user-token-12345"));
    assert!(!preview_stdout.contains("check-git.sh"));

    // Apply
    let apply = fixture.run_sr(&["install-hook", "claude", "--apply"]);
    assert_eq!(apply.status.code(), Some(0));

    // Verify file contents
    let content = fs::read_to_string(fixture.settings_path()).unwrap();
    let val: serde_json::Value = serde_json::from_str(&content).unwrap();

    assert_eq!(val["theme"], "dark");
    assert_eq!(val["secret_token"], "super-secret-user-token-12345");
    assert_eq!(
        val["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
        "linter.sh"
    );

    let user_prompt_submit = val["hooks"]["UserPromptSubmit"].as_array().unwrap();
    assert_eq!(user_prompt_submit.len(), 2);
    assert_eq!(user_prompt_submit[0]["matcher"], "git *");
    assert_eq!(user_prompt_submit[0]["hooks"][0]["command"], "check-git.sh");

    // Second hook is our managed hook
    assert!(user_prompt_submit[1]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .contains("hook claude"));
}

#[test]
fn preview_and_apply_uninstall() {
    let fixture = InstallerFixture::new();

    // Install first
    let install_out = fixture.run_sr(&["install-hook", "claude", "--apply"]);
    assert_eq!(install_out.status.code(), Some(0));

    // Preview uninstall
    let preview = fixture.run_sr(&["uninstall-hook", "claude"]);
    assert_eq!(preview.status.code(), Some(0));
    let preview_stdout = String::from_utf8_lossy(&preview.stdout);
    assert!(preview_stdout.contains("---"));
    assert!(preview_stdout.contains("+++"));
    assert!(preview_stdout.contains("- "));
    assert!(preview_stdout.contains("hook claude"));
    assert!(preview_stdout.contains("--apply to remove"));

    // Still installed
    let content_mid = fs::read_to_string(fixture.settings_path()).unwrap();
    let val_mid: serde_json::Value = serde_json::from_str(&content_mid).unwrap();
    assert!(val_mid.get("hooks").is_some());

    // Apply uninstall
    let apply = fixture.run_sr(&["uninstall-hook", "claude", "--apply"]);
    assert_eq!(apply.status.code(), Some(0));
    let apply_stdout = String::from_utf8_lossy(&apply.stdout);
    assert!(apply_stdout.contains("Successfully removed managed Claude hook"));

    // Verify removal
    let content_after = fs::read_to_string(fixture.settings_path()).unwrap();
    let val_after: serde_json::Value = serde_json::from_str(&content_after).unwrap();
    assert!(val_after.get("hooks").is_none());

    // Uninstalling again reports not installed
    let uninstall_again = fixture.run_sr(&["uninstall-hook", "claude", "--apply"]);
    assert_eq!(uninstall_again.status.code(), Some(0));
    let again_stdout = String::from_utf8_lossy(&uninstall_again.stdout);
    assert!(again_stdout.contains("No managed Claude hook entry found"));
}

#[test]
fn refuses_to_uninstall_modified_entry_and_displays_conflict() {
    let fixture = InstallerFixture::new();

    // Custom modified hook entry with modified timeout
    let settings = json!({
        "hooks": {
            "UserPromptSubmit": [
                {
                    "matcher": "",
                    "hooks": [
                        {
                            "type": "command",
                            "command": "/usr/local/bin/sr hook claude --custom-flag",
                            "timeout": 99
                        }
                    ]
                }
            ]
        }
    });

    fs::write(
        fixture.settings_path(),
        serde_json::to_string_pretty(&settings).unwrap(),
    )
    .unwrap();

    let out = fixture.run_sr(&["uninstall-hook", "claude", "--apply"]);
    assert_eq!(out.status.code(), Some(2), "must exit 2 on conflict");
    let output_text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        output_text.contains("modified externally")
            || output_text.contains("Refusing broad removal"),
        "output: {output_text}"
    );

    // Verify settings was NOT modified
    let content = fs::read_to_string(fixture.settings_path()).unwrap();
    let val: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_eq!(
        val["hooks"]["UserPromptSubmit"][0]["hooks"][0]["timeout"],
        99
    );
}

#[test]
fn refuses_malformed_settings_without_overwriting() {
    let fixture = InstallerFixture::new();

    let broken_json = b"{\n  \"broken\": [1, 2,\n";
    fs::write(fixture.settings_path(), broken_json).unwrap();

    let out = fixture.run_sr(&["install-hook", "claude", "--apply"]);
    assert_eq!(out.status.code(), Some(7), "must exit 7 (malformed input)");

    // Target must NOT be overwritten
    let content = fs::read(fixture.settings_path()).unwrap();
    assert_eq!(content, broken_json);
}

#[test]
fn reports_enterprise_read_only_restrictions() {
    if nix::unistd::geteuid().is_root() {
        eprintln!("skipping read-only restriction test when running as root");
        return;
    }

    let fixture = InstallerFixture::new();

    let settings = json!({"read_only": true});
    fs::write(
        fixture.settings_path(),
        serde_json::to_string_pretty(&settings).unwrap(),
    )
    .unwrap();

    // Make read-only
    let _ = fs::set_permissions(fixture.settings_path(), fs::Permissions::from_mode(0o400));
    let _ = fs::set_permissions(fixture.claude_dir(), fs::Permissions::from_mode(0o500));

    let out = fixture.run_sr(&["install-hook", "claude", "--apply"]);
    assert_eq!(out.status.code(), Some(2));
    let output_text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        output_text.contains("restricted")
            || output_text.contains("permission denied")
            || output_text.contains("enterprise"),
        "output: {output_text}"
    );

    // Restore permissions so cleanup succeeds
    let _ = fs::set_permissions(fixture.claude_dir(), fs::Permissions::from_mode(0o700));
    let _ = fs::set_permissions(fixture.settings_path(), fs::Permissions::from_mode(0o600));
}

#[test]
fn internal_deadline_exceeding_installed_budget_is_clamped() {
    let fixture = InstallerFixture::new();

    let hook_payload = json!({
        "hook_event_name": "UserPromptSubmit",
        "prompt": "test prompt",
        "prompt_id": "p-clamp-1",
        "session_id": "session-clamp-1",
        "cwd": fixture.workspace(),
    });

    // Pass timeout-ms 10000 (exceeding installed 3000ms budget)
    let mut child = Command::new(env!("CARGO_BIN_EXE_sr"))
        .env_clear()
        .env("HOME", fixture.home())
        .env("XDG_CONFIG_HOME", fixture.home().join(".config"))
        .current_dir(fixture.workspace())
        .arg("hook")
        .arg("claude")
        .arg("--timeout-ms")
        .arg("10000")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn sr hook claude");

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(&serde_json::to_vec(&hook_payload).unwrap());
    }

    let out = child.wait_with_output().expect("wait");
    assert_eq!(out.status.code(), Some(0));

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("exceeds installed hook budget") && stderr.contains("clamped"),
        "stderr must contain clamping diagnostic: {stderr}"
    );
}

#[test]
fn capabilities_lists_install_and_uninstall_as_implemented() {
    let fixture = InstallerFixture::new();
    let out = fixture.run_sr(&["capabilities", "--json"]);
    assert_eq!(out.status.code(), Some(0));

    let printed: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let commands = printed["commands"].as_array().unwrap();

    let install = commands
        .iter()
        .find(|c| c["name"] == "install-hook")
        .expect("install-hook in capabilities");
    assert_eq!(install["status"], "implemented");

    let uninstall = commands
        .iter()
        .find(|c| c["name"] == "uninstall-hook")
        .expect("uninstall-hook in capabilities");
    assert_eq!(uninstall["status"], "implemented");
}

/// The installed harness timeout must strictly exceed the internal deadline (sr-83cc).
///
/// Not "match" it. `sr`'s own clock starts at process entry, so process spawn, dynamic linking and
/// configuration reads all happen inside the harness timeout and outside the deadline; and the last
/// 200 ms of the deadline are reserved for writing the answer. An equal timeout can therefore kill
/// the run in precisely the window where being killed loses the reply rather than merely timing out.
#[test]
fn the_default_harness_timeout_strictly_exceeds_the_internal_deadline() {
    let deadline_ms = skillranker::limits::DEFAULT_INVOCATION_DEADLINE_MS;
    let outer_ms = u64::from(skillranker::installer::DEFAULT_HOOK_TIMEOUT_SECS) * 1_000;
    assert!(
        outer_ms > deadline_ms,
        "installed harness timeout {outer_ms} ms must exceed the internal deadline {deadline_ms} ms"
    );
    assert_eq!(
        skillranker::installer::minimum_hook_timeout_secs(deadline_ms),
        skillranker::installer::DEFAULT_HOOK_TIMEOUT_SECS,
        "the default should be exactly the minimum that bounds the deadline, so a later deadline \
         change is visible rather than absorbed"
    );
}

/// A timeout that cannot bound the deadline is refused, and the refusal carries the numbers.
#[test]
fn a_harness_timeout_that_cannot_bound_the_deadline_is_refused_with_its_numbers() {
    let deadline_ms = skillranker::limits::DEFAULT_INVOCATION_DEADLINE_MS;
    for too_tight in [0_u32, 1, 2, 3] {
        let refusal = skillranker::installer::check_hook_timeout(too_tight, deadline_ms)
            .expect_err("a timeout at or below the deadline must be refused");
        let message = refusal.to_string();
        assert!(
            message.contains(&too_tight.to_string()) && message.contains("3000"),
            "the refusal must state what was asked for and what the deadline is; got: {message}"
        );
    }
    assert!(skillranker::installer::check_hook_timeout(4, deadline_ms).is_ok());
    // A deadline raised past a whole second needs the next second, not merely one more than before.
    assert_eq!(skillranker::installer::minimum_hook_timeout_secs(3_200), 5);
    assert_eq!(skillranker::installer::minimum_hook_timeout_secs(1), 2);
}
