//! The capability registry against the real binary: every implemented command
//! runs, every planned command is refused, help and the registry agree, and
//! limits, exit codes and adapters come from their single sources.
use serde_json::Value;
use skillranker::adapter::{CLAUDE_CODE_ID, NORMALIZED_ID, foundation_capabilities};
use skillranker::capabilities::{IMPLEMENTED_COMMANDS, registry};
use skillranker::limits::ALL_DEFAULT_LIMITS;
use skillranker::output::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

// Intentionally retained: repository policy forbids automatic tree deletion.
fn home() -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "sr-capabilities-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir_all(root.join("workspace")).unwrap();
    root
}

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sr"))
        .env_clear()
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .current_dir(root.join("workspace"))
        .args(args)
        .output()
        .unwrap()
}

fn commands(document: &Value) -> Vec<(String, String)> {
    document["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["name"].as_str().unwrap().to_owned(),
                c["status"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[test]
fn the_binary_prints_the_registry_and_runs_exactly_the_implemented_commands() {
    let root = home();
    let output = run(&root, &["capabilities", "--json"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    let printed: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(printed, registry());
    assert_eq!(printed["schema"], "sr.capabilities.v1");
    let listed = commands(&printed);
    let implemented: Vec<&str> = listed
        .iter()
        .filter(|(_, status)| status == "implemented")
        .map(|(name, _)| name.as_str())
        .collect();
    for name in IMPLEMENTED_COMMANDS {
        assert!(implemented.contains(name), "{name}");
    }
    assert_eq!(implemented.len(), IMPLEMENTED_COMMANDS.len() + 2);
    // Every implemented subcommand answers --help.
    for name in IMPLEMENTED_COMMANDS {
        let help = run(&root, &[name, "--help"]);
        assert_eq!(help.status.code(), Some(0), "{name} --help");
    }
    // Every planned command is refused as invalid usage, never half-run.
    let planned: Vec<&str> = listed
        .iter()
        .filter(|(_, status)| status == "planned")
        .map(|(name, _)| name.as_str())
        .collect();
    assert!(!planned.contains(&"hook"));
    assert!(implemented.contains(&"hook"));
    assert!(planned.contains(&"tui"));
    assert!(!planned.contains(&"replay"));
    assert!(implemented.contains(&"replay"));
    for name in &planned {
        let refused = run(&root, &[name, "--json"]);
        assert_eq!(refused.status.code(), Some(2), "{name}");
        let error: Value = serde_json::from_slice(&refused.stdout).unwrap();
        assert_eq!(error["error"]["kind"], "invalid-usage", "{name}");
    }
    // Every foundation command is classified exactly once.
    let foundation = foundation_capabilities().unwrap();
    assert_eq!(listed.len(), foundation.planned_cli.len() + 2);
}

#[test]
fn help_names_implemented_commands_and_no_planned_ones() {
    let root = home();
    let help = String::from_utf8(run(&root, &["--help"]).stdout).unwrap();
    for name in IMPLEMENTED_COMMANDS {
        // Rank is also the bare command, written `sr [rank]`.
        let listed =
            help.contains(&format!("sr {name}")) || (*name == "rank" && help.contains("sr [rank]"));
        assert!(listed, "help lacks {name}");
    }
    for (name, status) in commands(&registry()) {
        if status == "planned" {
            assert!(
                !help.contains(&format!("sr {name}")),
                "help advertises {name}"
            );
        }
    }
    // Implemented flag is advertised in help.
    assert!(help.contains("--save-case"));
    for flag in registry()["planned_flags"].as_array().unwrap() {
        let flag_str = flag["flag"].as_str().unwrap();
        let command = flag["command"].as_str().unwrap();
        let command_help = String::from_utf8(run(&root, &[command, "--help"]).stdout).unwrap();
        assert!(
            !command_help.contains(flag_str),
            "{command} help advertises {flag_str}"
        );
    }
}

#[test]
fn planned_flags_contract() {
    let root = home();
    let planned_flags = registry()["planned_flags"].as_array().unwrap().clone();
    assert_eq!(planned_flags.len(), 3);
    for (command, flag, phase) in [
        ("doctor", "--descriptions", "p9"),
        ("eval", "--online", "p5"),
        ("eval", "--max-requests", "p5"),
    ] {
        assert!(planned_flags.iter().any(|entry| entry["command"] == command
            && entry["flag"] == flag
            && entry["earliest_phase"] == phase));
    }
    // Conflicts are still reported as conflicts.
    let conflict = run(
        &root,
        &["rank", "--dry-run", "--save-case", "case.json", "--json"],
    );
    assert_eq!(conflict.status.code(), Some(2));
}

#[test]
fn bare_sr_ranks_and_fails_without_a_session_source() {
    let root = home();
    let output = run(&root, &["--json"]);
    // Bare sr is rank; with no source it cannot select a session.
    assert_ne!(output.status.code(), Some(0));
    let error: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_ne!(error["error"]["kind"], "invalid-usage", "{error}");
}

#[test]
fn limits_exit_codes_adapters_and_features_come_from_their_sources() {
    let document = registry();
    let limits = document["limits"].as_array().unwrap();
    assert_eq!(limits.len(), ALL_DEFAULT_LIMITS.len());
    for (listed, limit) in limits.iter().zip(ALL_DEFAULT_LIMITS) {
        assert_eq!(listed["name"], limit.name);
        assert_eq!(listed["max"], limit.max());
    }
    let errors = document["exit_codes"]["errors"].as_array().unwrap();
    assert_eq!(errors.len(), ErrorKind::ALL.len());
    for (listed, kind) in errors.iter().zip(ErrorKind::ALL) {
        assert_eq!(listed["kind"], kind.as_str());
        assert_eq!(listed["exit_code"], kind.exit_code() as u8);
    }
    let adapters = document["adapters"].as_array().unwrap();
    let find = |id: &str| {
        adapters
            .iter()
            .find(|a| a["adapter_id"] == id)
            .unwrap()
            .clone()
    };
    // Normalized context is implemented; the Claude harness stays unverified,
    // with no tested version, so no native advice is claimed.
    assert_eq!(find(NORMALIZED_ID)["support"], "implemented");
    assert_eq!(find(CLAUDE_CODE_ID)["support"], "unverified");
    assert!(
        find(CLAUDE_CODE_ID)["tested_versions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(document["features"]["tui"]["implemented"], false);
    assert_eq!(
        document["features"]["tui"]["compiled"],
        cfg!(feature = "tui")
    );
}
