#![cfg(unix)]
//! `sr rank --session PATH` reads one exact cass session of this workspace.
//! A synthetic `cass` in the test HOME's `~/.local/bin` answers the qualified
//! capabilities, this workspace's session listing and one export. It is a
//! fixture, not evidence about an installed cass.
use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn caps() -> Value {
    json!({"crate_version":"0.8.0","api_version":1,"contract_version":"1","build_commit":"unknown",
           "global_flags":[{"name":"db"}],
           "features":["json_output","export_command","self_describing_capabilities"],
           "commands":[{"name":"export","arguments":[{"name":"path"},{"name":"source"},
                        {"name":"format","enum_values":["json"]},{"name":"include-tools"}]},
                       {"name":"sessions","arguments":[{"name":"workspace"},{"name":"limit"},{"name":"json"}]}]})
}

// Intentionally retained: repository policy forbids automatic tree deletion.
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "sr-cass-session-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        for name in ["alpha", "beta"] {
            let dir = root.join("workspace/.claude/skills").join(name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join("SKILL.md"),
                format!("---\ndescription: {name} helps with rust tests.\n---\nBody.\n"),
            )
            .unwrap();
        }
        fs::create_dir_all(root.join("home/.local/bin")).unwrap();
        fs::create_dir_all(root.join("config/sr")).unwrap();
        Self { root }
    }
    fn workspace(&self) -> PathBuf {
        fs::canonicalize(self.root.join("workspace")).unwrap()
    }
    fn session(&self) -> PathBuf {
        self.root.join("archive/session.jsonl")
    }
    /// Install a synthetic cass that lists `listed` for this workspace under
    /// `agent` and exports one user request.
    fn install_cass(&self, listed: &Path, agent: &str, request: &str) {
        let listing = json!({"sessions": [{"path": listed, "workspace": self.workspace(),
            "agent": agent, "source_id": "local", "origin_host": null}]});
        let export = json!([{"type": "user", "uuid": "u1", "parentUuid": null,
            "message": {"role": "user", "content": request}}]);
        let body = format!(
            "case \"$3\" in\ncapabilities) /bin/cat <<'CAPS'\n{}\nCAPS\n;;\n\
             sessions) /bin/cat <<'LIST'\n{}\nLIST\n;;\n\
             export) /bin/cat <<'EXPORT'\n{}\nEXPORT\n;;\nesac",
            caps(),
            listing,
            export
        );
        let path = self.root.join("home/.local/bin/cass");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        writeln!(file, "#!/bin/sh\n{body}").unwrap();
        file.set_permissions(fs::Permissions::from_mode(0o700))
            .unwrap();
    }
    fn rank(&self, session: &Path) -> (Option<i32>, Value) {
        let output = Command::new(env!("CARGO_BIN_EXE_sr"))
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .current_dir(self.workspace())
            .args(["rank", "--session", session.to_str().unwrap(), "--json"])
            .output()
            .unwrap();
        (
            output.status.code(),
            serde_json::from_slice(&output.stdout).unwrap(),
        )
    }
}

#[test]
fn a_cass_session_of_this_workspace_is_ranked() {
    let f = Fixture::new();
    f.install_cass(
        &f.session(),
        "claude",
        "Please use skill alpha to fix this.",
    );
    let (code, value) = f.rank(&f.session());
    assert_eq!(code, Some(0), "{value}");
    assert_eq!(value["decision"], "explicit", "{value}");
    assert_eq!(value["skills"][0]["invocation_name"], "alpha", "{value}");
}

#[test]
fn a_session_cass_does_not_record_for_this_workspace_is_refused() {
    let f = Fixture::new();
    f.install_cass(
        &f.session(),
        "claude",
        "Please use skill alpha to fix this.",
    );
    let (code, value) = f.rank(&f.root.join("archive/other.jsonl"));
    assert_eq!(code, Some(3), "{value}");
    assert_eq!(value["error"]["kind"], "missing-session", "{value}");
}

#[test]
fn a_cass_session_from_another_agent_needs_a_supplied_roster() {
    let f = Fixture::new();
    f.install_cass(&f.session(), "codex", "Please use skill alpha to fix this.");
    let (code, value) = f.rank(&f.session());
    assert_eq!(code, Some(5), "{value}");
    assert_eq!(value["error"]["kind"], "unusable-roster", "{value}");
}
