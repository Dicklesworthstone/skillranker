//! A command this build plans but has not implemented must say so.
//!
//! Planned commands name their phase and published inventory. The implemented
//! Claude hook instead keeps its quiet failure boundary, even without input.
use serde_json::Value;
use std::process::Command;

fn run(args: &[&str]) -> (Option<i32>, Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_sr"))
        .env_clear()
        .args(args)
        .output()
        .unwrap();
    let value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
    (output.status.code(), value)
}

#[test]
fn a_planned_command_names_the_phase_it_waits_for() {
    let (code, value) = run(&["calibrate", "--json"]);
    assert_eq!(code, Some(2), "{value}");
    assert_eq!(value["error"]["kind"], "invalid-usage", "{value}");
    let message = value["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("P8") && message.contains("sr capabilities"),
        "calibrate must name P8 and the inventory: {message}"
    );
}

#[test]
fn implemented_stats_reaches_storage_instead_of_a_planned_refusal() {
    // Explicitly select a missing store so this does not depend on the user's
    // real platform data. Populated-store success is exercised separately.
    let missing = std::env::temp_dir().join(format!(
        "sr-planned-stats-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let (code, value) = run(&["stats", "--json", "--dir", missing.to_str().unwrap()]);
    assert_eq!(code, Some(9), "{value}");
    assert_eq!(value["error"]["kind"], "storage-failure", "{value}");
    assert!(
        !value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("planned for phase")
    );
}

#[test]
fn the_documented_hook_invocation_without_input_fails_quietly() {
    let output = Command::new(env!("CARGO_BIN_EXE_sr"))
        .env_clear()
        .args(["hook", "claude", "--json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty(), "hook failures must remain silent");
}

#[test]
fn an_unknown_command_is_still_an_ordinary_usage_error() {
    // Only planned names get the phase message; a typo must not claim a phase.
    let (code, value) = run(&["hokk", "--json"]);
    assert_eq!(code, Some(2), "{value}");
    let message = value["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("use --help") && !message.contains("planned for phase"),
        "{message}"
    );
}

#[test]
fn an_implemented_command_is_never_reported_as_planned() {
    // capabilities lists rank, roster, doctor, demo, replay and ledger with a
    // phase too; none of them may be refused as unbuilt.
    for command in ["capabilities", "roster", "doctor"] {
        let (code, _) = run(&[command, "--json"]);
        assert_eq!(code, Some(0), "{command} is implemented");
    }
}

#[test]
fn planned_flags_name_their_phase_before_reading_inputs() {
    let (code, value) = run(&["doctor", "--descriptions", "--json"]);
    assert_eq!(code, Some(2), "{value}");
    let message = value["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("P9")
            && message.contains("--descriptions")
            && message.contains("sr capabilities"),
        "{message}"
    );
}

#[test]
fn planned_flag_spelling_is_not_a_global_argument_scan() {
    for args in [
        vec!["rank", "--descriptions", "--json"],
        vec!["eval", "--dataset", "/missing", "--explan", "--json"],
        vec!["eval", "--dataset=--explain", "--json"],
        vec!["eval", "--dataset", "/missing", "--seed", "--json"],
    ] {
        let (_, value) = run(&args);
        assert!(
            !value["error"]["message"]
                .as_str()
                .unwrap()
                .contains("planned for phase"),
            "{value}"
        );
    }
}
