//! Bundled private-session-free offline demonstrations.
//!
//! Satisfies contract boundary `p4_offline_demo` (sr-roadmap-l1i.5.16).
//!
//! Provides four synthetic demonstration cases:
//! 1. `useful`: A ranked decision with eligible skills, scores, and probabilities.
//! 2. `none`: An abstain decision where the sentinel none option wins.
//! 3. `explicit`: An explicit directive bypassing provider inference.
//! 4. `unavailable`: An unavailable decision explaining provider/auth prerequisites.
//!
//! All demonstrations are 100% offline, private-session-free, non-actionable,
//! and require no ambient configuration, credentials, or state persistence.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::output::{ContractError, OutputDocument, SCHEMA_VERSION};

/// Identifies one of the four bundled offline demonstration cases.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DemoCase {
    Useful,
    None,
    Explicit,
    Unavailable,
}

impl DemoCase {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "useful" => Some(Self::Useful),
            "none" => Some(Self::None),
            "explicit" => Some(Self::Explicit),
            "unavailable" => Some(Self::Unavailable),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Useful => "useful",
            Self::None => "none",
            Self::Explicit => "explicit",
            Self::Unavailable => "unavailable",
        }
    }

    pub const ALL: [Self; 4] = [Self::Useful, Self::None, Self::Explicit, Self::Unavailable];
}

/// Bundled synthetic context representation for demonstrations.
pub const SYNTHETIC_DEMO_CONTEXT: &str = r#"{
  "schema_version": 1,
  "producer": {"name": "claude_code", "version": "1.0.0"},
  "session": {"session_id": "demo-session-synthetic", "workspace_id": "demo-workspace"},
  "turns": [
    {
      "turn_id": "turn-001",
      "user_prompt": "Help me triage failing tests in the Rust test suite",
      "timestamp_ms": 1726700000000
    }
  ]
}"#;

/// Bundled synthetic roster representation for demonstrations.
pub const SYNTHETIC_DEMO_ROSTER: &str = r#"{
  "schema_version": 1,
  "skills": [
    {
      "skill_id": "s_01",
      "name": "rust-test-triage",
      "invocation_name": "rust-test-triage",
      "description": "Triage and fix failing Rust test suites",
      "path": ".claude/skills/rust-test-triage/SKILL.md",
      "content_hash": "0000000000000000000000000000000000000000000000000000000000000001"
    },
    {
      "skill_id": "s_02",
      "name": "rust-code-review",
      "invocation_name": "rust-code-review",
      "description": "Review Rust code for safety and style",
      "path": ".claude/skills/rust-code-review/SKILL.md",
      "content_hash": "0000000000000000000000000000000000000000000000000000000000000002"
    }
  ]
}"#;

/// Generates the raw JSON Value for a synthetic demo artifact envelope.
pub fn generate_demo_value(case: DemoCase) -> Value {
    let historical = match case {
        DemoCase::Useful => json!({
            "schema_version": SCHEMA_VERSION,
            "event_id": "demo-event-useful-001",
            "decision": "ranked",
            "reason": "eligible-candidates",
            "harness": "claude_code",
            "context_quality": "complete",
            "quality": {
                "prompt_complete": true,
                "task_anchor_known": true,
                "history_windowed": true,
                "attachments_omitted": false,
                "source_gaps": false
            },
            "roster": {
                "total": 2,
                "eligible": 2,
                "wide_candidates": 2,
                "shortlist": 2,
                "partial": false,
                "retrieval": "full",
                "provenance": {
                    "snapshot_id": "000000000000000000000000000000000000000000000000000000000000000a",
                    "policy_version": "ranking-v1",
                    "wide_set_id": "000000000000000000000000000000000000000000000000000000000000000b",
                    "rerank_set_id": "000000000000000000000000000000000000000000000000000000000000000c"
                }
            },
            "needs_skill": 0.74,
            "choice_confidence": 0.81,
            "none_probability": 0.1,
            "phase": "debugging",
            "skills": [
                {
                    "rank": 1,
                    "skill_id": "s_01",
                    "name": "rust-test-triage",
                    "invocation_name": "rust-test-triage",
                    "rank_score": 0.888889,
                    "rerank_probability": 0.6,
                    "wide_probability": 0.55,
                    "fits": 0.8,
                    "path": ".claude/skills/rust-test-triage/SKILL.md",
                    "content_hash": "0000000000000000000000000000000000000000000000000000000000000001"
                },
                {
                    "rank": 2,
                    "skill_id": "s_02",
                    "name": "rust-code-review",
                    "invocation_name": "rust-code-review",
                    "rank_score": 0.111111,
                    "rerank_probability": 0.3,
                    "wide_probability": 0.35,
                    "fits": 0.5,
                    "path": ".claude/skills/rust-code-review/SKILL.md",
                    "content_hash": "0000000000000000000000000000000000000000000000000000000000000002"
                }
            ],
            "omitted_rank_mass": 0.0,
            "cache": {
                "hit": false,
                "wide_hit": false,
                "rerank_hit": false,
                "age_ms": null,
                "stale": false
            },
            "model": {
                "requested": "jev-latest",
                "wide_returned": "jev-latest",
                "rerank_returned": "jev-latest",
                "immutable_revision": null
            },
            "usage": {
                "requests": 2,
                "http_attempts": 2,
                "input_tokens": 6400,
                "output_tokens": 480,
                "unknown_usage_attempts": 0
            },
            "persistence": "disabled",
            "warnings": [],
            "warnings_omitted": 0,
            "elapsed_ms": 720
        }),
        DemoCase::None => json!({
            "schema_version": SCHEMA_VERSION,
            "event_id": "demo-event-none-001",
            "decision": "abstain",
            "reason": "no-shortlist-match",
            "harness": "claude_code",
            "context_quality": "complete",
            "quality": {
                "prompt_complete": true,
                "task_anchor_known": true,
                "history_windowed": true,
                "attachments_omitted": false,
                "source_gaps": false
            },
            "roster": {
                "total": 3,
                "eligible": 3,
                "wide_candidates": 3,
                "shortlist": 2,
                "partial": false,
                "retrieval": "full",
                "provenance": {
                    "snapshot_id": "000000000000000000000000000000000000000000000000000000000000000a",
                    "policy_version": "ranking-v1",
                    "wide_set_id": "000000000000000000000000000000000000000000000000000000000000000b",
                    "rerank_set_id": "000000000000000000000000000000000000000000000000000000000000000c"
                }
            },
            "needs_skill": 0.15,
            "choice_confidence": 0.85,
            "none_probability": 0.85,
            "phase": null,
            "skills": [],
            "omitted_rank_mass": null,
            "cache": {
                "hit": false,
                "wide_hit": false,
                "rerank_hit": false,
                "age_ms": null,
                "stale": false
            },
            "model": {
                "requested": "jev-latest",
                "wide_returned": "jev-latest",
                "rerank_returned": "jev-latest",
                "immutable_revision": null
            },
            "usage": {
                "requests": 2,
                "http_attempts": 2,
                "input_tokens": 5800,
                "output_tokens": 420,
                "unknown_usage_attempts": 0
            },
            "persistence": "disabled",
            "warnings": [],
            "warnings_omitted": 0,
            "elapsed_ms": 650
        }),
        DemoCase::Explicit => json!({
            "schema_version": SCHEMA_VERSION,
            "event_id": "demo-event-explicit-001",
            "decision": "explicit",
            "reason": "user-required",
            "harness": "claude_code",
            "context_quality": "complete",
            "quality": {
                "prompt_complete": true,
                "task_anchor_known": true,
                "history_windowed": true,
                "attachments_omitted": false,
                "source_gaps": false
            },
            "roster": {
                "total": 2,
                "eligible": 2,
                "wide_candidates": 0,
                "shortlist": 0,
                "partial": false,
                "retrieval": "not-evaluated",
                "provenance": {
                    "snapshot_id": "000000000000000000000000000000000000000000000000000000000000000a",
                    "policy_version": "ranking-v1",
                    "wide_set_id": null,
                    "rerank_set_id": null
                }
            },
            "needs_skill": null,
            "choice_confidence": null,
            "none_probability": null,
            "phase": null,
            "skills": [
                {
                    "rank": 1,
                    "skill_id": "s_01",
                    "name": "rust-test-triage",
                    "invocation_name": "rust-test-triage",
                    "rank_score": null,
                    "rerank_probability": null,
                    "wide_probability": null,
                    "fits": null,
                    "path": ".claude/skills/rust-test-triage/SKILL.md",
                    "content_hash": "0000000000000000000000000000000000000000000000000000000000000001"
                }
            ],
            "omitted_rank_mass": null,
            "cache": {
                "hit": false,
                "wide_hit": false,
                "rerank_hit": false,
                "age_ms": null,
                "stale": false
            },
            "model": {
                "requested": null,
                "wide_returned": null,
                "rerank_returned": null,
                "immutable_revision": null
            },
            "usage": {
                "requests": 0,
                "http_attempts": 0,
                "input_tokens": 0,
                "output_tokens": 0,
                "unknown_usage_attempts": 0
            },
            "persistence": "disabled",
            "warnings": [],
            "warnings_omitted": 0,
            "elapsed_ms": 12
        }),
        DemoCase::Unavailable => json!({
            "schema_version": SCHEMA_VERSION,
            "decision": "unavailable",
            "error": {
                "code": 4,
                "kind": "credential-absent",
                "message": "Provider credentials are unavailable.",
                "hint": "Configure your own TypeSafe API key.",
                "retryable": false
            }
        }),
    };

    json!({
        "schema_version": SCHEMA_VERSION,
        "kind": "demo",
        "actionable": false,
        "run_status": "complete",
        "gate_status": "not-applicable",
        "evidence_origin": "synthetic",
        "completeness": {
            "cases_requested": 1,
            "cases_completed": 1,
            "stages_required": 1,
            "stages_completed": 1,
            "evidence_compatible": true
        },
        "historical": historical
    })
}

/// Generates a validated `OutputDocument` for the given demonstration case.
pub fn generate_demo_doc(case: DemoCase) -> Result<OutputDocument, ContractError> {
    let val = generate_demo_value(case);
    OutputDocument::from_value(val)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::{CliExit, OutputKind};

    #[test]
    fn all_four_cases_validate_as_demo_artifacts() {
        for case in DemoCase::ALL {
            let doc = generate_demo_doc(case).unwrap_or_else(|e| {
                panic!(
                    "failed to validate demo doc for case {}: {e:?}",
                    case.as_str()
                )
            });
            assert_eq!(
                doc.kind(),
                OutputKind::Artifact(crate::output::ArtifactKind::Demo)
            );
            assert_eq!(doc.exit_code(), CliExit::Success);

            let val = doc.as_value();
            assert_eq!(val["schema_version"], 1);
            assert_eq!(val["kind"], "demo");
            assert_eq!(val["actionable"], false);
            assert_eq!(val["run_status"], "complete");
            assert_eq!(val["gate_status"], "not-applicable");
            assert_eq!(val["evidence_origin"], "synthetic");
            assert!(val.get("historical").is_some());
        }
    }

    #[test]
    fn useful_case_structure() {
        let doc = generate_demo_doc(DemoCase::Useful).unwrap();
        let val = doc.as_value();
        let hist = &val["historical"];
        assert_eq!(hist["decision"], "ranked");
        let skills = hist["skills"].as_array().unwrap();
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0]["skill_id"], "s_01");
        assert_eq!(skills[0]["rank"], 1);
        assert!(skills[0]["rank_score"].as_f64().unwrap() > 0.8);
    }

    #[test]
    fn none_case_structure() {
        let doc = generate_demo_doc(DemoCase::None).unwrap();
        let val = doc.as_value();
        let hist = &val["historical"];
        assert_eq!(hist["decision"], "abstain");
        assert_eq!(hist["reason"], "no-shortlist-match");
        assert!(hist["skills"].as_array().unwrap().is_empty());
    }

    #[test]
    fn explicit_case_structure() {
        let doc = generate_demo_doc(DemoCase::Explicit).unwrap();
        let val = doc.as_value();
        let hist = &val["historical"];
        assert_eq!(hist["decision"], "explicit");
        assert_eq!(hist["reason"], "user-required");
        let skills = hist["skills"].as_array().unwrap();
        assert_eq!(skills.len(), 1);
        assert!(skills[0]["rank_score"].is_null());
        assert!(skills[0]["rerank_probability"].is_null());
    }

    #[test]
    fn unavailable_case_structure() {
        let doc = generate_demo_doc(DemoCase::Unavailable).unwrap();
        let val = doc.as_value();
        let hist = &val["historical"];
        assert_eq!(hist["decision"], "unavailable");
        assert_eq!(hist["error"]["kind"], "credential-absent");
        assert_eq!(hist["error"]["code"], 4);
    }
}
