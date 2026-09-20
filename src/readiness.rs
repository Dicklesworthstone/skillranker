//! Local readiness for `sr doctor`. Each prerequisite is its own state:
//! configuration, roster, credential presence, network consent, transport
//! evidence, ledger and hook mode. A failed check names one concrete next step.
//!
//! Doctor is local. It never sends a request, installs a hook, migrates
//! storage or edits configuration. Credential presence is never reported as
//! authentication, and no stored check establishes current provider health.

use crate::config::ResolvedConfig;
use crate::effects::EffectGate;
use crate::privacy::{ConsentSource, NetworkBlock, NetworkConsent};
use crate::roster::evidence::RosterEvidence;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const READINESS_COMMAND: &str = "doctor";
/// A recorded check dated this far past the local clock is not trusted.
pub const MAX_FUTURE_SKEW_MS: u64 = 5 * 60 * 1000;

/// What a live transport check was bound to. Any difference makes the
/// evidence historical only.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TransportIdentity {
    pub sr_version: String,
    /// Runtime and TLS trust configuration, such as `asupersync-0.5.0/native-roots`.
    pub runtime: String,
    /// Canonical endpoint origin, never with credentials or a path.
    pub endpoint_origin: String,
    pub model: String,
    pub timeout_ms: u64,
}

impl TransportIdentity {
    /// The identity this build and configuration would present to Jev.
    pub fn current(config: &ResolvedConfig, endpoint_origin: &str) -> Self {
        Self {
            sr_version: env!("CARGO_PKG_VERSION").to_owned(),
            runtime: "asupersync-0.5.0/native-roots".to_owned(),
            endpoint_origin: endpoint_origin.to_owned(),
            model: config.effective().model().as_str().to_owned(),
            timeout_ms: config.effective().timeout_ms(),
        }
    }
}

/// A separately authorized, budgeted live check, as its writer records it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TransportEvidence {
    pub checked_at_unix_ms: u64,
    /// What the check exercised, such as `wide+rerank`.
    pub scope: String,
    pub identity: TransportIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransportState {
    Untested,
    /// Historical: the provider answered then, for this identity. It does not
    /// establish current availability.
    PreviouslyVerified {
        checked_at_unix_ms: u64,
        scope: String,
    },
    /// Recorded for a different identity, or dated in the future.
    Invalidated {
        changed: Vec<&'static str>,
    },
}

pub fn assess_transport(
    evidence: Option<&TransportEvidence>,
    current: &TransportIdentity,
    now_unix_ms: u64,
) -> TransportState {
    let Some(evidence) = evidence else {
        return TransportState::Untested;
    };
    let recorded = &evidence.identity;
    let mut changed = Vec::new();
    for (name, differs) in [
        ("sr-version", recorded.sr_version != current.sr_version),
        ("runtime", recorded.runtime != current.runtime),
        (
            "endpoint",
            recorded.endpoint_origin != current.endpoint_origin,
        ),
        ("model", recorded.model != current.model),
        ("timeout", recorded.timeout_ms != current.timeout_ms),
        (
            "clock",
            evidence.checked_at_unix_ms > now_unix_ms.saturating_add(MAX_FUTURE_SKEW_MS),
        ),
    ] {
        if differs {
            changed.push(name);
        }
    }
    if changed.is_empty() {
        TransportState::PreviouslyVerified {
            checked_at_unix_ms: evidence.checked_at_unix_ms,
            scope: evidence.scope.clone(),
        }
    } else {
        TransportState::Invalidated { changed }
    }
}

/// Roster readiness from the workspace's resolved roster, or why it failed.
pub enum RosterCheck<'a> {
    Resolved(&'a RosterEvidence),
    Unusable,
    Timeout,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LedgerCheck {
    NotAvailable,
    Missing,
    Ready {
        cleanup_debt: Option<crate::storage::CleanupDebt>,
    },
    ReadOnly {
        cleanup_debt: Option<crate::storage::CleanupDebt>,
    },
}

pub struct Inputs<'a> {
    pub config: &'a ResolvedConfig,
    pub gate: EffectGate,
    pub roster: RosterCheck<'a>,
    pub transport: TransportState,
    pub ledger: LedgerCheck,
}

/// Implemented local commands a user can still run, whatever failed.
pub const LOCAL_COMMANDS: &[&str] = &[
    "sr doctor --config",
    "sr roster --json",
    "sr roster --snapshot FILE",
    "sr roster --diff FILE",
];

/// The readiness report. Every value is local: no request is sent.
pub fn report(inputs: &Inputs<'_>) -> Value {
    let config = inputs.config;
    let mut next_steps = Vec::new();
    let mut step = |check: &str, text: &str| {
        next_steps.push(json!({"check": check, "step": text}));
        Value::String(text.to_owned())
    };

    let configuration = json!({
        "state": "valid",
        "policy_fingerprint": config.effective().policy_fingerprint().as_str(),
    });

    let roster = match &inputs.roster {
        RosterCheck::Resolved(evidence) => {
            let counts = &evidence.counts;
            let (state, next) = if counts.skills == 0 {
                (
                    "empty",
                    step(
                        "roster",
                        "Add a skill directory containing SKILL.md under .claude/skills in this workspace or ~/.claude/skills, then run `sr roster --json`.",
                    ),
                )
            } else if counts.advisory == 0 {
                (
                    "no-advisory-candidates",
                    step(
                        "roster",
                        "Run `sr roster --json` to see why each skill is not offered as advice.",
                    ),
                )
            } else {
                ("ready", Value::Null)
            };
            json!({
                "state": state,
                "skills": counts.skills,
                "advisory": counts.advisory,
                "unverified": counts.unverified,
                "ambiguous": counts.ambiguous,
                "manual_only": counts.manual_only,
                "partial": evidence.partial,
                "source_causes": evidence.source_causes,
                "record_causes": evidence.record_causes,
                "snapshot": evidence.snapshot.as_str(),
                "next_step": next,
            })
        }
        RosterCheck::Unusable => json!({
            "state": "unusable",
            "next_step": step("roster", "Run `sr roster --json` to see which source failed."),
        }),
        RosterCheck::Timeout => json!({
            "state": "timeout",
            "next_step": step(
                "roster",
                "Retry; if discovery keeps timing out, inspect the roster with `sr roster --json`.",
            ),
        }),
    };

    let credential = if config.credential().is_some() {
        json!({"state": "present", "verified": false, "next_step": null})
    } else {
        json!({
            "state": "absent",
            "verified": false,
            "next_step": step(
                "credential",
                "Create your own API key in the TypeSafe console (https://console.typesafe.ai) and export TYPESAFE_API_KEY.",
            ),
        })
    };

    let network = match inputs.gate.network_consent(config) {
        NetworkConsent::Authorized(source) => json!({
            "state": "authorized",
            "source": match source {
                ConsentSource::AllowNetworkFlag => "allow-network-flag",
                ConsentSource::TrustedUserConfig => "trusted-user",
            },
            "next_step": null,
        }),
        NetworkConsent::NotAuthorized => json!({
            "state": "not-authorized",
            "next_step": step(
                "network",
                "Pass --allow-network for one run, or set network.enabled = true in trusted user configuration.",
            ),
        }),
        NetworkConsent::Blocked(block) => json!({
            "state": "blocked",
            "by": match block {
                NetworkBlock::Offline => "offline",
                NetworkBlock::DryRun => "dry-run",
            },
            "next_step": null,
        }),
    };

    let transport = match &inputs.transport {
        TransportState::Untested => json!({"state": "untested", "next_step": null}),
        TransportState::PreviouslyVerified {
            checked_at_unix_ms,
            scope,
        } => json!({
            "state": "previously-verified",
            "checked_at_unix_ms": checked_at_unix_ms,
            "scope": scope,
            "current": false,
            "next_step": null,
        }),
        TransportState::Invalidated { changed } => json!({
            "state": "invalidated",
            "changed": changed,
            "next_step": null,
        }),
    };

    let ledger = match &inputs.ledger {
        LedgerCheck::Ready { cleanup_debt } => {
            let has_debt = cleanup_debt.as_ref().map(|d| d.has_debt).unwrap_or(false);
            let next = if has_debt {
                step(
                    "ledger",
                    "Run `sr ledger prune --apply` to clean up expired events and reclaim space.",
                )
            } else {
                Value::Null
            };
            json!({
                "state": "ready",
                "blocks_ranking": false,
                "cleanup_debt": cleanup_debt,
                "next_step": next,
            })
        }
        LedgerCheck::ReadOnly { cleanup_debt } => json!({
            "state": "read-only",
            "blocks_ranking": false,
            "cleanup_debt": cleanup_debt,
            "next_step": null,
        }),
        LedgerCheck::Missing | LedgerCheck::NotAvailable => json!({
            "state": "not-available",
            "blocks_ranking": false,
            "cleanup_debt": null,
            "next_step": null,
        }),
    };

    let effective = config.effective();
    json!({
        "schema_version": 1,
        "command": READINESS_COMMAND,
        "scope": "local-readiness",
        "checks": {
            "configuration": configuration,
            // Doctor reads no session. The ranking command selects and checks
            // its source explicitly, so no input readiness is claimed here.
            "input": {"state": "not-evaluated", "next_step": null},
            "roster": roster,
            "credential": credential,
            "network": network,
            "transport": transport,
            "ledger": ledger,
            "hook": {
                "mode": effective.hook_mode().as_str(),
                "mode_sources": sources(config, crate::config::SettingKey::HookMode),
                "installation": "not-checked",
                "snoozes": "not-available",
            },
        },
        "next_steps": next_steps,
        "local_commands": LOCAL_COMMANDS,
    })
}

fn sources(config: &ResolvedConfig, key: crate::config::SettingKey) -> Vec<&'static str> {
    match config.source(key) {
        crate::config::ValueSource::Single(layer) => vec![layer.as_str()],
        crate::config::ValueSource::Union(layers) => {
            layers.iter().map(|layer| layer.as_str()).collect()
        }
    }
}
