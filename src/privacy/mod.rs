//! Trusted authority, disclosure and effect policy.
//!
//! Pure contracts: nothing here reads the environment, touches files, opens
//! sockets or creates state. The entry boundary supplies snapshots, and the
//! configuration resolver decides which layer may set each value. Read-time
//! checks (symlink targets, file types, origin canonicalization) remain with
//! the boundaries that perform those effects.

pub mod profile;
pub mod redaction;

pub use profile::{
    ProfileDisclosedFields, ProfileTrustError, count_disclosed_bytes, is_essential_tool_reference,
    resolve_context_profile, validate_project_profile,
};

use crate::identity::forbidden_identity_character;
use crate::output::ErrorKind;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Component, Path, PathBuf};

/// Bound for a credential taken from the process environment.
pub const MAX_CREDENTIAL_BYTES: usize = 4 * 1024;
/// Bound for one configured root before path normalization.
pub const MAX_ROOT_BYTES: usize = 4 * 1024;

/// Invocation effect flags. They come only from this process's command line;
/// no configuration file, environment variable or input artifact can set them.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EffectFlags {
    pub offline: bool,
    pub allow_network: bool,
    pub dry_run: bool,
    pub no_cache: bool,
    pub no_ledger: bool,
    pub no_persist: bool,
    pub save_case: bool,
}

/// Incompatible invocation modes are rejected, never silently corrected.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum FlagConflict {
    OfflineWithAllowNetwork,
    DryRunWithAllowNetwork,
    SaveCaseWithDryRun,
    SaveCaseWithNoPersist,
}

impl FlagConflict {
    pub const fn kind(self) -> ErrorKind {
        ErrorKind::InvalidUsage
    }

    pub const fn flags(self) -> (&'static str, &'static str) {
        match self {
            Self::OfflineWithAllowNetwork => ("--offline", "--allow-network"),
            Self::DryRunWithAllowNetwork => ("--dry-run", "--allow-network"),
            Self::SaveCaseWithDryRun => ("--save-case", "--dry-run"),
            Self::SaveCaseWithNoPersist => ("--save-case", "--no-persist"),
        }
    }
}

impl fmt::Display for FlagConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (a, b) = self.flags();
        write!(f, "{a} conflicts with {b}")
    }
}

/// The most restrictive flag that disabled an effect, for diagnostics only.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Restriction {
    DryRun,
    NoPersist,
    NoCache,
    NoLedger,
}

/// Disabled means neither reads nor writes; ordinary configuration reads are
/// not store access and remain permitted in every mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreAccess {
    Enabled,
    Disabled(Restriction),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkBlock {
    Offline,
    DryRun,
}

/// Validated, conflict-free effect flags with their combined restrictions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EffectPolicy {
    flags: EffectFlags,
}

impl EffectPolicy {
    /// Reports every conflict, not only the first.
    pub fn from_flags(flags: EffectFlags) -> Result<Self, Vec<FlagConflict>> {
        let mut conflicts = Vec::new();
        if flags.offline && flags.allow_network {
            conflicts.push(FlagConflict::OfflineWithAllowNetwork);
        }
        if flags.dry_run && flags.allow_network {
            conflicts.push(FlagConflict::DryRunWithAllowNetwork);
        }
        if flags.save_case && flags.dry_run {
            conflicts.push(FlagConflict::SaveCaseWithDryRun);
        }
        if flags.save_case && flags.no_persist {
            conflicts.push(FlagConflict::SaveCaseWithNoPersist);
        }
        if conflicts.is_empty() {
            Ok(Self { flags })
        } else {
            Err(conflicts)
        }
    }

    pub const fn flags(self) -> EffectFlags {
        self.flags
    }

    pub const fn response_cache(self) -> StoreAccess {
        self.first_restriction(self.flags.no_cache, Restriction::NoCache)
    }

    pub const fn ledger(self) -> StoreAccess {
        self.first_restriction(self.flags.no_ledger, Restriction::NoLedger)
    }

    /// Hash keys, locks, coordination leases, cooldowns and allowance accounting.
    pub const fn persistent_runtime_state(self) -> StoreAccess {
        if self.flags.dry_run {
            StoreAccess::Disabled(Restriction::DryRun)
        } else if self.flags.no_persist {
            StoreAccess::Disabled(Restriction::NoPersist)
        } else {
            StoreAccess::Enabled
        }
    }

    pub const fn case_capture(self) -> bool {
        self.flags.save_case
    }

    /// Offline still permits valid cache reads and local explicit resolution.
    pub const fn network_block(self) -> Option<NetworkBlock> {
        if self.flags.offline {
            Some(NetworkBlock::Offline)
        } else if self.flags.dry_run {
            Some(NetworkBlock::DryRun)
        } else {
            None
        }
    }

    const fn first_restriction(self, own_flag: bool, own: Restriction) -> StoreAccess {
        if self.flags.dry_run {
            StoreAccess::Disabled(Restriction::DryRun)
        } else if self.flags.no_persist {
            StoreAccess::Disabled(Restriction::NoPersist)
        } else if own_flag {
            StoreAccess::Disabled(own)
        } else {
            StoreAccess::Enabled
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConsentSource {
    AllowNetworkFlag,
    TrustedUserConfig,
}

/// Remote transmission authority. A credential's presence is not consent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkConsent {
    Authorized(ConsentSource),
    NotAuthorized,
    Blocked(NetworkBlock),
}

impl NetworkConsent {
    /// `trusted_user_enabled` must come from the trusted user layer. The
    /// configuration resolver is the only caller; project values cannot reach it.
    pub(crate) const fn derive(effects: EffectPolicy, trusted_user_enabled: bool) -> Self {
        if let Some(block) = effects.network_block() {
            Self::Blocked(block)
        } else if effects.flags.allow_network {
            Self::Authorized(ConsentSource::AllowNetworkFlag)
        } else if trusted_user_enabled {
            Self::Authorized(ConsentSource::TrustedUserConfig)
        } else {
            Self::NotAuthorized
        }
    }
}

/// A TypeSafe bearer credential. It is never serialized, displayed or cloned
/// into diagnostics; receipts carry only [`CredentialStatus`]. It deliberately
/// has no equality: nothing may compare credentials in non-constant time.
#[derive(Clone)]
pub struct ApiCredential(String);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialError {
    TooLong,
    InvalidCharacter,
}

impl ApiCredential {
    /// An empty variable is treated as absent, not as a credential.
    pub(crate) fn from_environment(value: String) -> Result<Option<Self>, CredentialError> {
        if value.is_empty() {
            return Ok(None);
        }
        if value.len() > MAX_CREDENTIAL_BYTES {
            return Err(CredentialError::TooLong);
        }
        if value.chars().any(forbidden_identity_character) {
            return Err(CredentialError::InvalidCharacter);
        }
        Ok(Some(Self(value)))
    }

    /// The single accessor, for an origin-scoped Authorization header only.
    pub fn expose_for_authorization_header(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiCredential(<redacted>)")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialStatus {
    Absent,
    PresentFromEnvironment,
}

/// Local refusal before any HTTP attempt is reserved or debited.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderAdmissionRefusal {
    Offline,
    DryRun,
    NetworkNotAuthorized,
    MissingCredential,
}

impl ProviderAdmissionRefusal {
    /// An offline refusal occurs only when no complete valid cached result
    /// exists, so it is a cache miss rather than an authorization failure.
    /// Dry run never plans a provider call; reaching this guard is a denial.
    pub const fn kind(self) -> ErrorKind {
        match self {
            Self::Offline => ErrorKind::CacheMiss,
            Self::DryRun | Self::NetworkNotAuthorized => ErrorKind::NetworkDenied,
            Self::MissingCredential => ErrorKind::Authentication,
        }
    }
}

impl fmt::Display for ProviderAdmissionRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Offline => "offline mode permits no network requests",
            Self::DryRun => "dry-run permits no network requests",
            Self::NetworkNotAuthorized => {
                "network transmission is not authorized; use --allow-network or trusted network.enabled"
            }
            Self::MissingCredential => "TYPESAFE_API_KEY is not set",
        })
    }
}

/// Consent is checked first so a blocked or unauthorized run never reports a
/// credential problem as its reason. Local explicit resolution and valid cache
/// output do not call this.
pub fn admit_provider_attempt(
    consent: NetworkConsent,
    credential: CredentialStatus,
) -> Result<ConsentSource, ProviderAdmissionRefusal> {
    let source = match consent {
        NetworkConsent::Blocked(NetworkBlock::Offline) => {
            return Err(ProviderAdmissionRefusal::Offline);
        }
        NetworkConsent::Blocked(NetworkBlock::DryRun) => {
            return Err(ProviderAdmissionRefusal::DryRun);
        }
        NetworkConsent::NotAuthorized => {
            return Err(ProviderAdmissionRefusal::NetworkNotAuthorized);
        }
        NetworkConsent::Authorized(source) => source,
    };
    match credential {
        CredentialStatus::PresentFromEnvironment => Ok(source),
        CredentialStatus::Absent => Err(ProviderAdmissionRefusal::MissingCredential),
    }
}

/// Ordered by disclosure: `Minimal < Standard`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextProfile {
    Minimal,
    #[default]
    Standard,
}

impl ContextProfile {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Standard => "standard",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "minimal" => Some(Self::Minimal),
            "standard" => Some(Self::Standard),
            _ => None,
        }
    }
}

impl fmt::Display for ContextProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RootError {
    Empty,
    TooLong,
    InvalidCharacter,
    AbsoluteNotAllowed,
    RelativeNotAllowed,
    EscapesWorkspace,
    ParentComponent,
}

impl fmt::Display for RootError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "root is empty",
            Self::TooLong => "root exceeds its byte limit",
            Self::InvalidCharacter => "root contains a control or bidirectional character",
            Self::AbsoluteNotAllowed => "an absolute root requires trusted user configuration",
            Self::RelativeNotAllowed => "root must be an absolute path",
            Self::EscapesWorkspace => "root escapes the workspace",
            Self::ParentComponent => "absolute root must not contain '..'",
        })
    }
}

fn check_root_text(text: &str) -> Result<(), RootError> {
    if text.is_empty() {
        return Err(RootError::Empty);
    }
    if text.len() > MAX_ROOT_BYTES {
        return Err(RootError::TooLong);
    }
    // Spaces are legitimate in directory names; controls and bidi overrides are not.
    if text
        .chars()
        .any(|c| c != ' ' && forbidden_identity_character(c))
    {
        return Err(RootError::InvalidCharacter);
    }
    Ok(())
}

/// A lexically normalized path inside the workspace; empty means the root.
/// Symlink containment is enforced separately when the reader opens it.
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct WorkspaceRelativeRoot(PathBuf);

impl WorkspaceRelativeRoot {
    pub fn parse(text: &str) -> Result<Self, RootError> {
        check_root_text(text)?;
        let mut normalized = PathBuf::new();
        let mut depth = 0usize;
        for component in Path::new(text).components() {
            match component {
                Component::Prefix(_) | Component::RootDir => {
                    return Err(RootError::AbsoluteNotAllowed);
                }
                Component::CurDir => {}
                Component::ParentDir => {
                    if depth == 0 {
                        return Err(RootError::EscapesWorkspace);
                    }
                    normalized.pop();
                    depth -= 1;
                }
                Component::Normal(part) => {
                    normalized.push(part);
                    depth += 1;
                }
            }
        }
        Ok(Self(normalized))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

impl fmt::Debug for WorkspaceRelativeRoot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WorkspaceRelativeRoot(<private>)")
    }
}

/// An absolute root granted by trusted user configuration.
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct TrustedAbsoluteRoot(PathBuf);

impl TrustedAbsoluteRoot {
    pub fn parse(text: &str) -> Result<Self, RootError> {
        check_root_text(text)?;
        let path = Path::new(text);
        if !path.is_absolute() {
            return Err(RootError::RelativeNotAllowed);
        }
        // '..' cannot be resolved lexically without following symlinks.
        if path.components().any(|c| c == Component::ParentDir) {
            return Err(RootError::ParentComponent);
        }
        Ok(Self(path.components().collect()))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

impl fmt::Debug for TrustedAbsoluteRoot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TrustedAbsoluteRoot(<private>)")
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum SkillRoot {
    WorkspaceRelative(WorkspaceRelativeRoot),
    TrustedAbsolute(TrustedAbsoluteRoot),
}
