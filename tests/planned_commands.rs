//! A command this build plans but has not implemented must say so.
//!
//! README documents `sr hook claude`, and `sr capabilities` correctly reports
//! `hook` as planned for P6, but the parser has no such subcommand, so the
//! invocation was refused as "Unsupported or conflicting arguments". A user
//! following the documentation could not tell an unbuilt command from a typo.
//! The refusal now names the phase and points at the published inventory.
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
    for (command, phase) in [("hook", "P6"), ("stats", "P5"), ("calibrate", "P8")] {
        let (code, value) = run(&[command, "--json"]);
        assert_eq!(code, Some(2), "{command}: {value}");
        assert_eq!(
            value["error"]["kind"], "invalid-usage",
            "{command}: {value}"
        );
        let message = value["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains(phase) && message.contains("sr capabilities"),
            "{command} must name {phase} and the inventory: {message}"
        );
    }
}

#[test]
fn the_documented_hook_invocation_is_refused_with_its_phase() {
    // The exact line README gives a reader.
    let (code, value) = run(&["hook", "claude", "--json"]);
    assert_eq!(code, Some(2), "{value}");
    let message = value["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("P6"), "{message}");
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
