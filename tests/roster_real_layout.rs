#![cfg(unix)]
//! What a real, untidy skills root reports (sr-gl5j).
//!
//! A skills directory on a working machine contains more than skills: editor
//! notes, marker files, and the occasional skill whose SKILL.md outgrew the
//! 256 KiB read bound. Those entries must be excluded with a cause that says
//! what is actually wrong. An oversized file was reported as `unreadable`,
//! which sends its owner looking for a permissions fault that does not exist.
//! Every case runs offline: no provider is contacted.
use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use skillranker::limits::SKILL_FILE_BYTES;

static NEXT: AtomicU64 = AtomicU64::new(0);

// Intentionally retained: repository policy forbids automatic tree deletion.
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "sr-roster-layout-{}-{}-{}",
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
        f.skill("rust-tester", "Runs and repairs failing rust tests.");
        f
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }
    fn workspace(&self) -> PathBuf {
        self.root.join("workspace")
    }
    fn skills(&self) -> PathBuf {
        self.home().join(".claude/skills")
    }

    fn skill(&self, name: &str, description: &str) {
        let dir = self.skills().join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            format!("---\ndescription: {description}\n---\nBody.\n"),
        )
        .unwrap();
    }

    /// A skill whose body exceeds the per-skill read bound. Real skills do get
    /// this big: the one that prompted this test is 508 KB.
    fn oversized_skill(&self, name: &str) {
        let dir = self.skills().join(name);
        fs::create_dir_all(&dir).unwrap();
        let body = "x".repeat(SKILL_FILE_BYTES.max() + 1024);
        fs::write(
            dir.join("SKILL.md"),
            format!("---\ndescription: {name} is enormous.\n---\n{body}\n"),
        )
        .unwrap();
    }

    fn roster(&self) -> Value {
        let output = Command::new(env!("CARGO_BIN_EXE_sr"))
            .env_clear()
            .env("HOME", self.home())
            .env("XDG_CONFIG_HOME", self.home().join(".config"))
            .current_dir(self.workspace())
            .args(["roster", "--json"])
            .output()
            .unwrap();
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

#[test]
fn an_oversized_skill_is_reported_as_oversized_not_unreadable() {
    let f = Fixture::new();
    f.oversized_skill("enormous");
    let value = f.roster();
    let causes = &value["record_causes"];
    assert_eq!(causes["oversized"], 1, "{value}");
    assert!(
        causes.get("unreadable").is_none(),
        "nothing here is unreadable: {value}"
    );
    // The neighbouring valid skill survives the exclusion.
    assert_eq!(value["counts"]["skills"], 1, "{value}");
}

#[test]
fn an_untidy_skills_root_still_lists_its_valid_skills() {
    let f = Fixture::new();
    f.skill("notes-writer", "Drafts release notes from git history.");
    f.oversized_skill("enormous");
    // The kind of debris a real skills directory accumulates.
    fs::write(f.skills().join("RESEARCH_NOTES.md"), "not a skill\n").unwrap();
    fs::write(f.skills().join("PARENT_DIR.info"), "marker\n").unwrap();
    fs::create_dir_all(f.skills().join("empty-dir")).unwrap();
    let value = f.roster();
    assert_eq!(value["counts"]["skills"], 2, "{value}");
    let names: Vec<&str> = value["records"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["invocation_name"].as_str())
        .collect();
    assert!(
        names.contains(&"rust-tester") && names.contains(&"notes-writer"),
        "{value}"
    );
    // The oversized entry is the only excluded record, and it is not called
    // unreadable. Loose files beside the skill directories are not candidates
    // at all under the direct layout, so they contribute no record cause.
    let causes = &value["record_causes"];
    assert_eq!(causes["oversized"], 1, "{value}");
    assert!(causes.get("unreadable").is_none(), "{value}");
    // Debris still leaves the listing incomplete rather than silently whole.
    assert_eq!(value["partial"], true, "{value}");
}
