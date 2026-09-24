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
    // `stats` used to stand here and graduated in the 6.13 work, which is the point
    // of this case: a command that becomes real must leave this list, and the test
    // fails loudly if it does not.
    for (command, phase) in [("eval", "P5"), ("calibrate", "P8")] {
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
