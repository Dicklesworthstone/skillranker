//! Real binary checks of `sr roster --snapshot` and `--diff`.
use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "sr-roster-snapshot-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("workspace/.claude/skills")).unwrap();
        std::fs::create_dir_all(root.join("home/.claude/skills")).unwrap();
        // Snapshot targets must sit in a private directory, whatever the umask.
        std::fs::set_permissions(
            root.join("workspace"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        Self { root }
    }
    fn workspace(&self) -> PathBuf {
        self.root.join("workspace")
    }
    fn project_root(&self) -> PathBuf {
        self.root.join("workspace/.claude/skills")
    }
    fn personal_root(&self) -> PathBuf {
        self.root.join("home/.claude/skills")
    }
    fn write(&self, root: PathBuf, name: &str, extra: &str, body: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\ndescription: A skill.\n{extra}---\n{body}\n"),
        )
        .unwrap();
    }
    fn run_in(&self, cwd: PathBuf, home: PathBuf, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_sr"))
            .env_clear()
            .env("HOME", home)
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap()
    }
    fn run(&self, args: &[&str]) -> Output {
        self.run_in(self.workspace(), self.root.join("home"), args)
    }
    fn ok(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(output.stderr.is_empty());
        serde_json::from_slice(&output.stdout).unwrap()
    }
    fn fails(&self, args: &[&str], code: i32, kind: &str) {
        let output = self.run(args);
        assert_eq!(output.status.code(), Some(code), "{args:?}");
        let error: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(error["error"]["kind"], kind, "{args:?}");
        assert!(!String::from_utf8_lossy(&output.stdout).contains(self.root.to_str().unwrap()));
    }
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn a_snapshot_is_owner_only_path_free_and_never_overwrites() {
    let f = Fixture::new();
    f.write(f.project_root(), "alpha", "", "Alpha body.");
    f.write(f.personal_root(), "beta", "", "Beta body.");
    let receipt = f.ok(&["roster", "--snapshot", "snap.json"]);
    assert_eq!(receipt["exported"], true);
    assert_eq!(receipt["records"], 2);
    let path = f.workspace().join("snap.json");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let bytes = std::fs::read(&path).unwrap();
    let text = String::from_utf8(bytes.clone()).unwrap();
    assert!(
        !text.contains(f.root.to_str().unwrap()),
        "no local path is stored"
    );
    let saved: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(saved["schema"], "sr.roster-snapshot.v1");
    assert_eq!(saved["records"].as_array().unwrap().len(), 2);
    // A second export to the same target is refused and leaves it intact.
    f.fails(&["roster", "--snapshot", "snap.json"], 9, "storage-failure");
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    // An unchanged tree compares as empty.
    let diff = f.ok(&["roster", "--diff", "snap.json"]);
    for key in [
        "added",
        "removed",
        "unconfirmed_removals",
        "renamed",
        "content_changed",
        "restriction_changed",
        "status_changed",
    ] {
        assert!(diff[key].as_array().unwrap().is_empty(), "{key}");
    }
    assert_eq!(
        std::fs::read(&path).unwrap(),
        bytes,
        "a diff never modifies its source"
    );
}

#[test]
fn a_diff_reports_renames_replacements_restrictions_additions_and_removals() {
    let f = Fixture::new();
    for name in ["alpha", "beta", "gamma", "epsilon"] {
        f.write(f.project_root(), name, "", &format!("{name} body."));
    }
    f.ok(&["roster", "--snapshot", "before.json"]);
    // Rename a directory without changing its bytes.
    std::fs::rename(
        f.project_root().join("alpha"),
        f.project_root().join("alpha2"),
    )
    .unwrap();
    // Replace a same-name skill's content.
    f.write(f.project_root(), "beta", "", "A different body.");
    // Withdraw agent invocation.
    f.write(
        f.project_root(),
        "gamma",
        "disable-model-invocation: true\n",
        "gamma body.",
    );
    // Add one and remove one.
    f.write(f.project_root(), "delta", "", "delta body.");
    std::fs::remove_file(f.project_root().join("epsilon/SKILL.md")).unwrap();
    let diff = f.ok(&["roster", "--diff", "before.json"]);
    assert_eq!(diff["schema"], "sr.roster-diff.v1");
    assert_eq!(diff["renamed"], serde_json::json!([["alpha", "alpha2"]]));
    let changed = strings(&diff["content_changed"]);
    assert!(changed.contains(&"beta".to_owned()) && changed.contains(&"gamma".to_owned()));
    assert_eq!(strings(&diff["restriction_changed"]), vec!["gamma"]);
    assert_eq!(strings(&diff["added"]), vec!["delta"]);
    // The project root was fully observed, so this removal is confirmed.
    assert_eq!(strings(&diff["removed"]), vec!["epsilon"]);
    assert!(diff["unconfirmed_removals"].as_array().unwrap().is_empty());
    // Plugin and managed sources were never enumerated.
    assert_eq!(diff["complete"], false);
}

#[test]
fn an_unreadable_root_makes_removals_unconfirmed() {
    let f = Fixture::new();
    f.write(f.personal_root(), "kept", "", "Kept body.");
    f.write(f.project_root(), "local", "", "Local body.");
    f.ok(&["roster", "--snapshot", "before.json"]);
    // The personal root becomes unreadable: its skills cannot be declared deleted.
    std::fs::rename(f.personal_root(), f.root.join("home/.claude/skills-moved")).unwrap();
    std::fs::write(f.personal_root(), "not a directory").unwrap();
    let diff = f.ok(&["roster", "--diff", "before.json"]);
    assert_eq!(strings(&diff["unconfirmed_removals"]), vec!["kept"]);
    assert!(diff["removed"].as_array().unwrap().is_empty());
}

#[test]
fn a_different_workspace_or_home_is_an_incompatible_namespace() {
    let f = Fixture::new();
    f.write(f.project_root(), "alpha", "", "Alpha body.");
    f.ok(&["roster", "--snapshot", "snap.json"]);
    let other = f.root.join("other");
    std::fs::create_dir_all(other.join(".claude/skills")).unwrap();
    std::fs::copy(f.workspace().join("snap.json"), other.join("snap.json")).unwrap();
    let output = f.run_in(
        other,
        f.root.join("home"),
        &["roster", "--diff", "snap.json"],
    );
    assert_eq!(output.status.code(), Some(5));
    let error: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(error["error"]["kind"], "unusable-roster");
    let elsewhere = f.root.join("home2");
    std::fs::create_dir_all(elsewhere.join(".claude/skills")).unwrap();
    let output = f.run_in(f.workspace(), elsewhere, &["roster", "--diff", "snap.json"]);
    assert_eq!(output.status.code(), Some(5));
}

#[test]
fn imported_paths_unknown_fields_and_unsafe_files_are_refused() {
    let f = Fixture::new();
    f.write(f.project_root(), "alpha", "", "Alpha body.");
    f.ok(&["roster", "--snapshot", "snap.json"]);
    let original = std::fs::read_to_string(f.workspace().join("snap.json")).unwrap();
    let mut saved: Value = serde_json::from_str(&original).unwrap();
    // A record that tries to carry a path is refused, never followed.
    saved["records"][0]["path"] = "/etc/passwd".into();
    std::fs::write(f.workspace().join("pathy.json"), saved.to_string()).unwrap();
    f.fails(&["roster", "--diff", "pathy.json"], 7, "malformed-input");
    let mut unknown: Value = serde_json::from_str(&original).unwrap();
    unknown["restore"] = true.into();
    std::fs::write(f.workspace().join("unknown.json"), unknown.to_string()).unwrap();
    f.fails(&["roster", "--diff", "unknown.json"], 7, "malformed-input");
    let duplicated = original.replacen("\"schema\"", "\"schema\": \"x\", \"schema\"", 1);
    std::fs::write(f.workspace().join("dup.json"), duplicated).unwrap();
    f.fails(&["roster", "--diff", "dup.json"], 7, "malformed-input");
    let mut schema: Value = serde_json::from_str(&original).unwrap();
    schema["schema"] = "sr.roster-snapshot.v9".into();
    std::fs::write(f.workspace().join("schema.json"), schema.to_string()).unwrap();
    f.fails(&["roster", "--diff", "schema.json"], 5, "unusable-roster");
    // A symlink leaving the manifest's directory and a FIFO are not read.
    let outside = f.root.join("outside.json");
    std::fs::write(&outside, &original).unwrap();
    std::os::unix::fs::symlink(&outside, f.workspace().join("link.json")).unwrap();
    f.fails(&["roster", "--diff", "link.json"], 7, "unsupported-input");
    nix::unistd::mkfifo(
        &f.workspace().join("fifo.json"),
        nix::sys::stat::Mode::S_IRWXU,
    )
    .unwrap();
    f.fails(&["roster", "--diff", "fifo.json"], 7, "unsupported-input");
    assert_eq!(
        std::fs::read_to_string(f.workspace().join("snap.json")).unwrap(),
        original
    );
}

#[test]
fn snapshot_and_diff_do_not_mix_with_paging() {
    let f = Fixture::new();
    f.fails(
        &["roster", "--snapshot", "a.json", "--limit", "5"],
        2,
        "invalid-usage",
    );
    f.fails(
        &["roster", "--diff", "a.json", "--cursor", "x"],
        2,
        "invalid-usage",
    );
    f.fails(
        &["roster", "--snapshot", "a.json", "--diff", "b.json"],
        2,
        "invalid-usage",
    );
    assert!(!f.workspace().join("a.json").exists());
}
