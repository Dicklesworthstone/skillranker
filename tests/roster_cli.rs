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
        let root = std::env::temp_dir().join(format!(
            "sr-roster-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
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
