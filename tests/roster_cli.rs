//! Real binary checks of `sr roster`: complete records, snapshot-bound pages,
//! and clean streams, over actual skill trees.
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        // Trees are retained, so a reused PID must never reuse an old tree.
        let root = std::fs::canonicalize("/tmp").unwrap().join(format!(
            "sr-roster-cli-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .unwrap();
        std::fs::create_dir_all(root.join("workspace/.claude/skills")).unwrap();
        std::fs::create_dir_all(root.join("home/.claude/skills")).unwrap();
        Self { root }
    }
    fn project(&self, name: &str, extra: &str) {
        self.skill("workspace", name, extra);
    }
    fn personal(&self, name: &str, extra: &str) {
        self.skill("home", name, extra);
    }
    fn skill(&self, base: &str, name: &str, extra: &str) {
        let dir = self.root.join(base).join(".claude/skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {name} skill.\n{extra}---\nBody.\n"),
        )
        .unwrap();
    }
    fn run(&self, args: &[&str], environment: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_sr"));
        cmd.env_clear()
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("home/.config"))
            .current_dir(self.root.join("workspace"))
            .args(args);
        for (name, value) in environment {
            cmd.env(name, value);
        }
        cmd.output().unwrap()
    }
    fn json(&self, args: &[&str]) -> Value {
        let output = self.run(args, &[]);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty(), "stderr stays empty on success");
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

fn names(page: &Value) -> Vec<String> {
    page["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["invocation_name"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn records_report_status_counts_and_partial_sources_without_a_key() {
    let f = Fixture::new();
    f.project("alpha", "");
    f.project("manual", "disable-model-invocation: true\n");
    f.project("shared", "");
    f.personal("shared", "");
    let output = f.run(
        &["roster", "--json"],
        &[("TYPESAFE_API_KEY", "privatecanary")],
    );
    assert_eq!(output.status.code(), Some(0));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("privatecanary"));
    let page: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(page["schema"], "sr.roster-listing.v1");
    assert_eq!(page["total"], 4);
    assert_eq!(page["counts"]["bindings"], 4);
    // Plugin and managed sources are not enumerated, so the roster is partial.
    assert_eq!(page["partial"], true);
    assert_eq!(page["source_causes"]["source-not-enumerated"], 2);
    assert!(page["next_cursor"].is_null());
    let records = page["records"].as_array().unwrap();
    let by_name = |name: &str| -> Vec<&Value> {
        records
            .iter()
            .filter(|r| r["invocation_name"] == name)
            .collect()
    };
    // The Claude adapter is unverified: no precedence is claimed, so the
    // same-name pair is ambiguous and the rest are unverified.
    assert!(by_name("shared").iter().all(|r| r["status"] == "ambiguous"));
    assert_eq!(by_name("alpha")[0]["status"], "unverified");
    let manual = by_name("manual")[0];
    assert_eq!(manual["agent_invocable"], false);
    assert_eq!(manual["user_invocable"], true);
    // Records are in stable skill-ID order.
    let ids: Vec<&str> = records
        .iter()
        .map(|r| r["skill_id"].as_str().unwrap())
        .collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(ids, sorted);
}

#[test]
fn concatenated_pages_equal_the_whole_snapshot() {
    let f = Fixture::new();
    for i in 0..300 {
        f.project(&format!("skill{i:03}"), "");
    }
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    let mut snapshots = BTreeSet::new();
    let mut pages = 0;
    loop {
        let mut args = vec!["roster", "--json", "--limit", "128"];
        if let Some(token) = &cursor {
            args.push("--cursor");
            args.push(token);
        }
        let page = f.json(&args);
        pages += 1;
        snapshots.insert(page["snapshot"].as_str().unwrap().to_owned());
        assert_eq!(page["start"], seen.len());
        seen.extend(names(&page));
        match page["next_cursor"].as_str() {
            Some(next) => cursor = Some(next.to_owned()),
            None => break,
        }
    }
    assert_eq!(pages, 3);
    assert_eq!(snapshots.len(), 1, "every page belongs to one snapshot");
    assert_eq!(seen.len(), 300);
    let unique: BTreeSet<_> = seen.iter().collect();
    assert_eq!(unique.len(), 300, "no record repeats or goes missing");
    // Smaller pages concatenate to the same sequence.
    let first = f.json(&["roster", "--json", "--limit", "100"]);
    let second = f.json(&[
        "roster",
        "--json",
        "--limit",
        "100",
        "--cursor",
        first["next_cursor"].as_str().unwrap(),
    ]);
    let mut prefix = names(&first);
    prefix.extend(names(&second));
    assert_eq!(prefix, seen[..200]);
}

#[test]
fn a_change_between_pages_requires_a_restart() {
    let f = Fixture::new();
    for i in 0..5 {
        f.project(&format!("skill{i}"), "");
    }
    let first = f.json(&["roster", "--json", "--limit", "2"]);
    let token = first["next_cursor"].as_str().unwrap().to_owned();
    f.project("late", "");
    let output = f.run(
        &["roster", "--json", "--limit", "2", "--cursor", &token],
        &[],
    );
    assert_eq!(output.status.code(), Some(5));
    let error: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(error["error"]["kind"], "roster-changed");
    assert_eq!(error["decision"], "unavailable");
    // Restarting without the stale cursor lists the new snapshot.
    let restarted = f.json(&["roster", "--json", "--limit", "2"]);
    assert_eq!(restarted["total"], 6);
    assert_ne!(restarted["snapshot"], first["snapshot"]);
}

#[test]
fn bad_limits_and_cursors_are_usage_errors_on_clean_streams() {
    let f = Fixture::new();
    f.project("alpha", "");
    f.project("beta", "");
    let bad = [
        vec!["roster", "--json", "--limit", "0"],
        vec!["roster", "--json", "--limit", "129"],
        vec!["roster", "--json", "--limit", "many"],
        vec!["roster", "--json", "--cursor", "garbage"],
        vec!["roster", "--json", "--cursor", "r1.zz.1"],
        vec!["roster", "--json", "--unknown"],
    ];
    for args in bad {
        let output = f.run(&args, &[]);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        let error: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(error["error"]["kind"], "invalid-usage", "{args:?}");
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(!text.contains(f.root.to_str().unwrap()));
    }
    // A well-formed cursor past the end is refused, not an empty success.
    let page = f.json(&["roster", "--json", "--limit", "1"]);
    let snapshot = page["snapshot"].as_str().unwrap();
    let output = f.run(
        &["roster", "--json", "--cursor", &format!("r1.{snapshot}.9")],
        &[],
    );
    assert_eq!(output.status.code(), Some(2));
    // Without --json on a pipe the output is still machine-readable JSON.
    let plain = f.run(&["roster"], &[]);
    assert_eq!(plain.status.code(), Some(0));
    let value: Value = serde_json::from_slice(&plain.stdout).unwrap();
    assert_eq!(value["total"], 2);
}

#[test]
fn an_empty_workspace_lists_nothing_and_help_needs_no_discovery() {
    let f = Fixture::new();
    let page = f.json(&["roster", "--json"]);
    assert_eq!(page["total"], 0);
    assert!(page["records"].as_array().unwrap().is_empty());
    assert!(page["next_cursor"].is_null());
    let help = f.run(&["roster", "--help"], &[]);
    assert_eq!(help.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&help.stdout).contains("sr roster"));
}

fn custom_skill(f: &Fixture, root: &str, name: &str) {
    let directory = f.root.join(root).join(name);
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: Custom local skill.\n---\nBody.\n"),
    )
    .unwrap();
}

fn project_roots(f: &Fixture, text: &str) {
    std::fs::create_dir_all(f.root.join("workspace/.sr")).unwrap();
    std::fs::write(f.root.join("workspace/.sr/config.toml"), text).unwrap();
}

#[test]
fn configured_project_and_user_roots_reach_roster_doctor_and_snapshot() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    std::fs::set_permissions(&f.root, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::set_permissions(
        f.root.join("workspace"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    f.project("default", "");
    custom_skill(&f, "workspace/custom", "local-extra");
    custom_skill(&f, "external", "user-extra");
    project_roots(&f, "[roster]\nroots=['custom', '.claude/skills']\n");
    std::fs::create_dir_all(f.root.join("home/.config/sr")).unwrap();
    std::fs::write(
        f.root.join("home/.config/sr/config.toml"),
        format!(
            "[roster]\nroots=['{}']\n",
            f.root.join("external").display()
        ),
    )
    .unwrap();
    let page = f.json(&["roster", "--json"]);
    assert_eq!(
        names(&page).into_iter().collect::<BTreeSet<_>>(),
        BTreeSet::from(["default".into(), "local-extra".into(), "user-extra".into()])
    );
    assert_eq!(
        page["counts"]["bindings"], 3,
        "default root alias must not add a binding"
    );
    assert!(
        page["records"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["status"] == "unverified")
    );
    assert!(!page.to_string().contains(f.root.to_str().unwrap()));
    let doctor = f.json(&["doctor", "--json"]);
    assert_eq!(doctor["checks"]["roster"]["skills"], 3);
    // Configured roots are rankable, as rank suggests them, and are
    // reported unverified.
    assert_eq!(doctor["checks"]["roster"]["advisory"], 3);
    assert_eq!(doctor["checks"]["roster"]["visibility"], "unverified");
    project_roots(&f, "[roster]\nroots=['.claude/skills', 'custom']\n");
    let reordered = f.json(&["roster", "--json"]);
    assert_eq!(page["snapshot"], reordered["snapshot"]);
    assert_eq!(page["records"], reordered["records"]);
    let snapshot = f.run(&["roster", "--snapshot", "configured.json"], &[]);
    assert_eq!(
        snapshot.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&snapshot.stderr)
    );
    let saved: Value =
        serde_json::from_slice(&std::fs::read(f.root.join("workspace/configured.json")).unwrap())
            .unwrap();
    assert!(saved.to_string().contains("local-extra"));
    let comparison = f.run(&["roster", "--diff", "configured.json"], &[]);
    assert_eq!(comparison.status.code(), Some(0));
}

#[test]
fn project_root_symlink_cannot_grant_external_reads_but_contained_root_works() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    custom_skill(&f, "external", "must-not-be-read");
    custom_skill(&f, "workspace/inside", "allowed");
    symlink(f.root.join("external"), f.root.join("workspace/escape")).unwrap();
    symlink("inside", f.root.join("workspace/alias")).unwrap();
    project_roots(&f, "[roster]\nroots=['escape', 'alias']\n");
    let page = f.json(&["roster", "--json"]);
    assert_eq!(names(&page), vec!["allowed"]);
    assert!(!page.to_string().contains("must-not-be-read"));
    assert_eq!(page["partial"], true);
    assert!(
        page["source_causes"]["root-unreadable"]
            .as_u64()
            .unwrap_or(0)
            > 0
    );
}

#[test]
fn roster_validates_configuration_instead_of_silently_ignoring_it() {
    let f = Fixture::new();
    f.project("default", "");
    for text in [
        "[roster]\nroots=['../external']\n",
        "[roster]\nroots=['/external']\n",
        "[network]\nenabled=true\n",
    ] {
        project_roots(&f, text);
        let output = f.run(&["roster", "--json"], &[]);
        assert_eq!(output.status.code(), Some(2));
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["decision"], "unavailable");
    }
}
