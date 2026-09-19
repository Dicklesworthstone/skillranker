//! The capability registry of this build: which commands run, which remain
//! planned for a later phase, supported adapters and their evidence, schema
//! versions, compiled features, resource limits and exit codes.
//!
//! `adapter::foundation_capabilities` stays the P0 contract fixture. This
//! registry reports the running build and is what `sr capabilities --json`
//! prints. It claims nothing about phase acceptance, provider availability or
//! an installed harness version.

use crate::adapter::{CapabilitiesDocument, PhaseGate, foundation_capabilities};
use crate::limits::{ALL_DEFAULT_LIMITS, LimitUnit};
use crate::output::ErrorKind;
use serde_json::{Value, json};

pub const CAPABILITIES_SCHEMA: &str = "sr.capabilities.v1";

/// Commands this binary accepts and runs. Every other planned command is
/// rejected as invalid usage.
pub const IMPLEMENTED_COMMANDS: &[&str] = &["capabilities", "demo", "doctor", "rank", "roster"];

/// Accepted by the parser for conflict checking, but refused with
/// `invalid-usage` until their phase ships.
pub const PLANNED_RANK_FLAGS: &[(&str, PhaseGate)] = &[("save-case", PhaseGate::P5)];

const fn unit_name(unit: LimitUnit) -> &'static str {
    match unit {
        LimitUnit::Bytes => "bytes",
        LimitUnit::Records => "records",
        LimitUnit::Items => "items",
        LimitUnit::Depth => "depth",
        LimitUnit::UnicodeScalars => "unicode-scalars",
        LimitUnit::Milliseconds => "milliseconds",
        LimitUnit::Attempts => "attempts",
        LimitUnit::LogicalRequests => "logical-requests",
    }
}

fn phase_name(phase: PhaseGate) -> &'static str {
    match phase {
        PhaseGate::P0 => "p0",
        PhaseGate::P1 => "p1",
        PhaseGate::P2 => "p2",
        PhaseGate::P3 => "p3",
        PhaseGate::P4 => "p4",
        PhaseGate::P5 => "p5",
        PhaseGate::P6 => "p6",
        PhaseGate::P7 => "p7",
        PhaseGate::P8 => "p8",
        PhaseGate::P9 => "p9",
    }
}

/// The registry for this build. The foundation document supplies the command
/// inventory, phases and adapter evidence, so the two cannot drift apart.
pub fn registry() -> Value {
    let foundation: CapabilitiesDocument =
        foundation_capabilities().expect("the built-in foundation capabilities are valid");
    let mut commands = vec![
        json!({"name": "help", "status": "implemented"}),
        json!({"name": "version", "status": "implemented"}),
    ];
    for command in &foundation.planned_cli {
        let status = if IMPLEMENTED_COMMANDS.contains(&command.name.as_str()) {
            "implemented"
        } else {
            "planned"
        };
        commands.push(json!({
            "name": command.name,
            "status": status,
            "earliest_phase": phase_name(command.earliest_phase),
        }));
    }
    let planned_flags: Vec<Value> = PLANNED_RANK_FLAGS
        .iter()
        .map(|(flag, phase)| {
            json!({
                "command": "rank",
                "flag": format!("--{flag}"),
                "earliest_phase": phase_name(*phase),
            })
        })
        .collect();
    let limits: Vec<Value> = ALL_DEFAULT_LIMITS
        .iter()
        .map(|limit| json!({"name": limit.name, "unit": unit_name(limit.unit), "max": limit.max()}))
        .collect();
    let errors: Vec<Value> = ErrorKind::ALL
        .iter()
        .map(|kind| json!({"kind": kind.as_str(), "exit_code": kind.exit_code() as u8}))
        .collect();
    json!({
        "schema": CAPABILITIES_SCHEMA,
        "sr_version": env!("CARGO_PKG_VERSION"),
        "commands": commands,
        "planned_flags": planned_flags,
        "adapters": foundation.adapters,
        "adapter_contract_version": foundation.adapter_contract_version,
        "schemas": {
            "output": crate::output::SCHEMA_VERSION,
            "configuration": crate::config::CONFIG_SCHEMA_VERSION,
            "roster_import": crate::roster::import::ROSTER_SCHEMA,
            "roster_listing": crate::roster::inspect::LISTING_SCHEMA,
            "roster_snapshot": crate::roster::snapshot::SNAPSHOT_SCHEMA,
            "roster_diff": crate::roster::snapshot::DIFF_SCHEMA,
            "retrieval": crate::roster::retrieval::RETRIEVAL_SCHEMA,
            "wide_questions": crate::jev::wide::WIDE_POLICY_VERSION,
            "rerank_questions": crate::jev::rerank::RERANK_POLICY_VERSION,
        },
        "features": {
            // Reserved boundary: no TUI implementation ships in any build yet.
            "tui": {"compiled": cfg!(feature = "tui"), "implemented": false},
        },
        "limits": limits,
        "exit_codes": {"success": 0, "errors": errors},
    })
}
