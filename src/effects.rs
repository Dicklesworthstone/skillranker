//! Central invocation effect gate.
//!
//! One validated [`EffectPolicy`] taken from this process's command line derives
//! every consumer's restriction, so no boundary re-reads raw flags or builds its
//! own interpretation of them:
//!
//! | Boundary | Derived value |
//! | --- | --- |
//! | Context sources and unverified children (cass) | [`EffectGate::source_policy`] |
//! | Response cache | [`EffectGate::response_cache`], `open_cache` |
//! | Ledger and ingestion cursors | [`EffectGate::ledger`], [`EffectGate::history`] |
//! | Hash key, locks, leases, cooldowns, allowance accounting | [`EffectGate::runtime_state`] |
//! | Provider network | [`EffectGate::network_consent`] |
//!
//! Flags combine restrictively. Offline controls network access only; it does
//! not imply no persistence. Dry run implies no persistence. Configuration and
//! input files are not state stores and are read in every mode.

use crate::config::ResolvedConfig;
use crate::context::source::SourcePolicy;
use crate::privacy::{
    EffectFlags, EffectPolicy, FlagConflict, NetworkBlock, NetworkConsent, Restriction, StoreAccess,
};
use serde::Serialize;

/// The command family a gate serves.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Scope {
    /// Ranking and other commands that may use stores, children and Jev.
    Rank,
    /// Local-only inspection. It never runs unverified children and never
    /// plans a provider call, whatever its flags.
    LocalInspection,
}

/// Persistent historical evidence (ledger observations, cursors, judgments).
/// `Withheld` means unknown. It is never an empty history: callers must not
/// turn it into negative observations, zero loads, or a missing prior.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum History {
    Available,
    Withheld(Restriction),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EffectGate {
    policy: EffectPolicy,
    scope: Scope,
}

impl EffectGate {
    /// Incompatible flags are rejected, never corrected. Every conflict is
    /// reported.
    pub fn new(flags: EffectFlags, scope: Scope) -> Result<Self, Vec<FlagConflict>> {
        EffectPolicy::from_flags(flags).map(|policy| Self { policy, scope })
    }

    pub const fn policy(self) -> EffectPolicy {
        self.policy
    }

    pub const fn scope(self) -> Scope {
        self.scope
    }

    /// The only production derivation of the context source policy. Offline,
    /// dry run and local inspection refuse cass before any child starts.
    pub const fn source_policy(self) -> SourcePolicy {
        let flags = self.policy.flags();
        SourcePolicy {
            offline: flags.offline,
            dry_run: flags.dry_run,
            local_only: matches!(self.scope, Scope::LocalInspection),
            allow_network: flags.allow_network,
        }
    }

    /// Whether an unverified child such as cass may run. The verified Git
    /// signal path is not an unverified child and stays available.
    pub const fn unverified_children(self) -> bool {
        self.source_policy().cass_allowed()
    }

    pub const fn response_cache(self) -> StoreAccess {
        self.policy.response_cache()
    }

    pub const fn ledger(self) -> StoreAccess {
        self.policy.ledger()
    }

    /// Hash key, locks, leases, cooldowns and allowance accounting.
    pub const fn runtime_state(self) -> StoreAccess {
        self.policy.persistent_runtime_state()
    }

    pub const fn history(self) -> History {
        match self.policy.ledger() {
            StoreAccess::Enabled => History::Available,
            StoreAccess::Disabled(restriction) => History::Withheld(restriction),
        }
    }

    /// Leases and cooldowns shared between processes need persistent runtime
    /// state. Without it they are process-local, and output must say so.
    pub const fn cross_process_coordination(self) -> bool {
        matches!(self.runtime_state(), StoreAccess::Enabled)
    }

    /// A stateless preview reads no stores, so it equals the corresponding
    /// `--no-persist` run rather than a run with hidden learned state.
    pub const fn stateless(self) -> bool {
        matches!(self.response_cache(), StoreAccess::Disabled(_))
            && matches!(self.ledger(), StoreAccess::Disabled(_))
            && matches!(self.runtime_state(), StoreAccess::Disabled(_))
    }

    /// Network consent from this gate and trusted configuration. Local
    /// inspection is treated as offline: it fails closed if a provider path is
    /// ever reached.
    pub const fn network_consent(self, config: &ResolvedConfig) -> NetworkConsent {
        match self.scope {
            Scope::LocalInspection => NetworkConsent::Blocked(NetworkBlock::Offline),
            Scope::Rank => config.network_consent(self.policy),
        }
    }

    /// The effective mode, for output that must not be mistaken for a run with
    /// hidden persistent state.
    pub fn receipt(self) -> EffectReceipt {
        let network = match self.scope {
            Scope::LocalInspection => NetworkEffect::Blocked {
                by: "local-inspection",
            },
            Scope::Rank => match self.policy.network_block() {
                Some(NetworkBlock::Offline) => NetworkEffect::Blocked { by: "offline" },
                Some(NetworkBlock::DryRun) => NetworkEffect::Blocked { by: "dry-run" },
                None => NetworkEffect::ConsentRequired,
            },
        };
        EffectReceipt {
            response_cache: StoreEffect::from(self.response_cache()),
            ledger: StoreEffect::from(self.ledger()),
            runtime_state: StoreEffect::from(self.runtime_state()),
            network,
            unverified_children: self.unverified_children(),
            cross_process_coordination: self.cross_process_coordination(),
            stateless: self.stateless(),
        }
    }

    /// Cache access for a caller that would otherwise request `requested`.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub const fn cache_access(
        self,
        requested: crate::storage::CacheAccess,
    ) -> crate::storage::CacheAccess {
        match self.response_cache() {
            StoreAccess::Enabled => requested,
            StoreAccess::Disabled(_) => crate::storage::CacheAccess::Disabled,
        }
    }

    /// Open the response cache through this gate. A disabled cache returns
    /// before any path resolution, engine inspection or file access.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub fn open_cache(
        self,
        invocation: &crate::runtime::ProcessInvocation,
        cx: &asupersync::Cx,
        requested: crate::storage::CacheAccess,
        location: crate::storage::CacheLocation,
    ) -> Result<crate::storage::CacheOpen, crate::storage::StoreError> {
        crate::storage::open_cache(invocation, cx, self.cache_access(requested), location)
    }
}

const fn restriction_name(restriction: Restriction) -> &'static str {
    match restriction {
        Restriction::DryRun => "dry-run",
        Restriction::NoPersist => "no-persist",
        Restriction::NoCache => "no-cache",
        Restriction::NoLedger => "no-ledger",
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum StoreEffect {
    Enabled,
    Disabled { by: &'static str },
}

impl From<StoreAccess> for StoreEffect {
    fn from(access: StoreAccess) -> Self {
        match access {
            StoreAccess::Enabled => Self::Enabled,
            StoreAccess::Disabled(restriction) => Self::Disabled {
                by: restriction_name(restriction),
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum NetworkEffect {
    /// Not blocked by a flag; trusted consent and a credential are still required.
    ConsentRequired,
    Blocked {
        by: &'static str,
    },
}

/// Serializable effective mode. It names disabled stores and their flag, and
/// never contains paths, credentials or configuration values.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct EffectReceipt {
    pub response_cache: StoreEffect,
    pub ledger: StoreEffect,
    pub runtime_state: StoreEffect,
    pub network: NetworkEffect,
    pub unverified_children: bool,
    pub cross_process_coordination: bool,
    pub stateless: bool,
}
