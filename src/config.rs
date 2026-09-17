//! Trusted configuration layers, the frozen v1 key registry and effective-policy
//! receipts.
//!
//! Pure contract. File and environment reads, TOML decoding and clap parsing
//! happen at the command boundary, which hands over already-bounded entries and
//! must finish this validation before discovery, networking or mutation.
//! Nothing here creates state, so resolving or revalidating configuration is
//! permitted under `--dry-run` and `--no-persist`.

use crate::identity::{ContentHash, MAX_ID_BYTES, forbidden_identity_character};
use crate::limits::{
    DEFAULT_INVOCATION_DEADLINE_MS, DEFAULT_OUTPUT_CLEANUP_RESERVE_MS, RECENT_NORMALIZED_MESSAGES,
    RENDERED_CONTEXT_SCALARS,
};
use crate::output::ErrorKind;
use crate::privacy::{
    ApiCredential, ContextProfile, CredentialError, CredentialStatus, EffectPolicy, NetworkConsent,
    RootError, SkillRoot, TrustedAbsoluteRoot, WorkspaceRelativeRoot,
};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fmt;

pub const CONFIG_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_MODEL: &str = "jev-latest";
pub const MIN_TIMEOUT_MS: u64 = DEFAULT_OUTPUT_CLEANUP_RESERVE_MS + 1;
pub const MAX_TIMEOUT_MS: u64 = 60_000;
pub const MAX_RANK_SIZE: u32 = 32;

/// Recognized entries per layer; unrelated environment variables do not count.
pub const MAX_LAYER_ENTRIES: usize = 256;
pub const MAX_KEY_BYTES: usize = 128;
pub const MAX_STRING_VALUE_BYTES: usize = 4 * 1024;
pub const MAX_MODEL_BYTES: usize = 128;
pub const MAX_LIST_ITEMS: usize = 128;
pub const MAX_ROOTS: usize = 32;
/// Diagnostics beyond this bound are counted, not listed.
pub const MAX_REPORTED_ISSUES: usize = 32;

/// Ordered by precedence: later layers override earlier ones where permitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum ConfigLayer {
    BuiltIn,
    TrustedUser,
    Project,
    Environment,
    Cli,
}

impl ConfigLayer {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BuiltIn => "built-in",
            Self::TrustedUser => "trusted-user",
            Self::Project => "project",
            Self::Environment => "environment",
            Self::Cli => "cli",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LayerRule {
    Forbidden,
    Allowed,
    /// May only keep or narrow the value resolved from lower layers.
    RestrictOnly,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Sensitivity {
    Ordinary,
    DisclosureVolume,
    NetworkConsent,
    Credential,
    Routing,
    TranscriptAccess,
    SkillRootAccess,
    AdviceInjection,
    /// Recognized so a project attempt is reported precisely; not settable in v1.
    Reserved,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ValueKind {
    Bool,
    Count { min: u32, max: u32 },
    Millis { min: u64, max: u64 },
    Unit { min: f64, max: f64 },
    ContextProfile,
    HookMode,
    ModelName,
    Endpoint,
    Credential,
    SkillReferences,
    TrustedAbsoluteRoots,
    SkillRoots,
    Reserved,
}

impl ValueKind {
    const fn merges_as_union(self) -> bool {
        matches!(self, Self::SkillReferences | Self::SkillRoots)
    }
}

macro_rules! setting_keys {
    ($($variant:ident),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
        pub enum SettingKey { $($variant),+ }

        impl SettingKey {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
        }
    };
}

setting_keys! {
    NetworkEnabled,
    NetworkProxy,
    TypesafeApiKey,
    TypesafeEndpoint,
    ProviderModel,
    HookMode,
    ContextProfile,
    ContextNoTools,
    ContextMessages,
    ContextBudgetChars,
    ContextTranscriptRoots,
    RankingTop,
    RankingShortlist,
    RankingGate,
    RankingFits,
    RankingWFit,
    RankingWPrior,
    RankingWPhase,
    RankingTimeoutMs,
    RankingExcludeSkills,
    RosterRoots,
    PrivacyRedaction,
    PrivacyRawRetention,
}

impl SettingKey {
    pub fn spec(self) -> &'static KeySpec {
        &KEY_SPECS[self as usize]
    }

    pub fn path(self) -> &'static str {
        self.spec().path
    }

    pub fn from_path(path: &str) -> Option<Self> {
        KEY_SPECS.iter().find(|s| s.path == path).map(|s| s.key)
    }

    pub fn from_environment_name(name: &str) -> Option<Self> {
        KEY_SPECS
            .iter()
            .find(|s| s.environment == Some(name))
            .map(|s| s.key)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KeySpec {
    pub key: SettingKey,
    /// Dotted configuration path; CLI entries use it too, bound to `cli_flag`.
    pub path: &'static str,
    pub environment: Option<&'static str>,
    pub cli_flag: Option<&'static str>,
    pub kind: ValueKind,
    pub sensitivity: Sensitivity,
    pub trusted_user: LayerRule,
    pub project: LayerRule,
    pub environment_rule: LayerRule,
    pub cli: LayerRule,
    /// Writable by calibration apply/rollback into trusted user configuration.
    pub managed_policy: bool,
}

impl KeySpec {
    pub const fn rule(&self, layer: ConfigLayer) -> LayerRule {
        match layer {
            ConfigLayer::BuiltIn => LayerRule::Forbidden,
            ConfigLayer::TrustedUser => self.trusted_user,
            ConfigLayer::Project => self.project,
            ConfigLayer::Environment => self.environment_rule,
            ConfigLayer::Cli => self.cli,
        }
    }
}

use LayerRule::{Allowed, Forbidden, RestrictOnly};

const fn spec(
    key: SettingKey,
    path: &'static str,
    kind: ValueKind,
    sensitivity: Sensitivity,
    rules: [LayerRule; 4],
) -> KeySpec {
    KeySpec {
        key,
        path,
        environment: None,
        cli_flag: None,
        kind,
        sensitivity,
        trusted_user: rules[0],
        project: rules[1],
        environment_rule: rules[2],
        cli: rules[3],
        managed_policy: false,
    }
}

const fn env(mut s: KeySpec, name: &'static str) -> KeySpec {
    s.environment = Some(name);
    s
}

const fn flag(mut s: KeySpec, name: &'static str) -> KeySpec {
    s.cli_flag = Some(name);
    s
}

const fn managed(mut s: KeySpec) -> KeySpec {
    s.managed_policy = true;
    s
}

const RESERVED: [LayerRule; 4] = [Forbidden; 4];

const fn unit(min: f64, max: f64) -> ValueKind {
    ValueKind::Unit { min, max }
}

/// Rules are `[trusted user, project, environment, cli]`, in `SettingKey` order.
#[rustfmt::skip]
pub static KEY_SPECS: [KeySpec; 23] = {
    use Sensitivity as S;
    use SettingKey as K;
    use ValueKind as V;
    let messages = V::Count { min: 1, max: RECENT_NORMALIZED_MESSAGES.max() as u32 };
    let budget = V::Count { min: 1, max: RENDERED_CONTEXT_SCALARS.max() as u32 };
    let rank_size = V::Count { min: 1, max: MAX_RANK_SIZE };
    let timeout = V::Millis { min: MIN_TIMEOUT_MS, max: MAX_TIMEOUT_MS };
    //                                                                                                    user          project       environment   cli
    [
        spec(K::NetworkEnabled, "network.enabled", V::Bool, S::NetworkConsent,                               [Allowed,      Forbidden,    Forbidden,    Forbidden]),
        spec(K::NetworkProxy, "network.proxy", V::Reserved, S::Reserved,                                     RESERVED),
        env(spec(K::TypesafeApiKey, "typesafe.api_key", V::Credential, S::Credential,                        [Forbidden,    Forbidden,    Allowed,      Forbidden]), "TYPESAFE_API_KEY"),
        env(spec(K::TypesafeEndpoint, "typesafe.endpoint", V::Endpoint, S::Routing,                          [Forbidden,    Forbidden,    Allowed,      Forbidden]), "TYPESAFE_ENDPOINT"),
        env(spec(K::ProviderModel, "provider.model", V::ModelName, S::Routing,                               [Allowed,      Forbidden,    Allowed,      Forbidden]), "SR_MODEL"),
        flag(spec(K::HookMode, "hook.mode", V::HookMode, S::AdviceInjection,                                 [Allowed,      Forbidden,    Forbidden,    RestrictOnly]), "--shadow"),
        flag(spec(K::ContextProfile, "context.profile", V::ContextProfile, S::DisclosureVolume,              [Allowed,      RestrictOnly, Forbidden,    Allowed]), "--context-profile"),
        flag(spec(K::ContextNoTools, "context.no_tools", V::Bool, S::DisclosureVolume,                       [Allowed,      RestrictOnly, Forbidden,    RestrictOnly]), "--no-tools"),
        flag(env(spec(K::ContextMessages, "context.messages", messages, S::DisclosureVolume,                 [Allowed,      RestrictOnly, Allowed,      Allowed]), "SR_MESSAGES"), "--messages"),
        flag(env(spec(K::ContextBudgetChars, "context.budget_chars", budget, S::DisclosureVolume,            [Allowed,      RestrictOnly, Allowed,      Allowed]), "SR_BUDGET_CHARS"), "--budget-chars"),
        spec(K::ContextTranscriptRoots, "context.transcript_roots", V::TrustedAbsoluteRoots, S::TranscriptAccess, [Allowed,  Forbidden,    Forbidden,    Forbidden]),
        flag(env(spec(K::RankingTop, "ranking.top", rank_size, S::Ordinary,                                  [Allowed,      Allowed,      Allowed,      Allowed]), "SR_TOP"), "--top"),
        flag(env(spec(K::RankingShortlist, "ranking.shortlist", rank_size, S::Ordinary,                      [Allowed,      Allowed,      Allowed,      Allowed]), "SR_SHORTLIST"), "--shortlist"),
        managed(flag(env(spec(K::RankingGate, "ranking.gate", unit(0.0, 1.0), S::Ordinary,                   [Allowed,      Allowed,      Allowed,      Allowed]), "SR_GATE"), "--gate")),
        managed(flag(env(spec(K::RankingFits, "ranking.fits", unit(0.0, 1.0), S::Ordinary,                   [Allowed,      Allowed,      Allowed,      Allowed]), "SR_FITS"), "--fits")),
        managed(spec(K::RankingWFit, "ranking.w_fit", unit(0.0, 4.0), S::Ordinary,                           [Allowed,      Allowed,      Forbidden,    Forbidden])),
        managed(spec(K::RankingWPrior, "ranking.w_prior", unit(0.0, 0.5), S::Ordinary,                       [Allowed,      Allowed,      Forbidden,    Forbidden])),
        managed(spec(K::RankingWPhase, "ranking.w_phase", unit(0.0, 1.0), S::Ordinary,                       [Allowed,      Allowed,      Forbidden,    Forbidden])),
        flag(env(spec(K::RankingTimeoutMs, "ranking.timeout_ms", timeout, S::Ordinary,                       [Allowed,      Forbidden,    Allowed,      Allowed]), "SR_TIMEOUT_MS"), "--timeout-ms"),
        spec(K::RankingExcludeSkills, "ranking.exclude_skills", V::SkillReferences, S::Ordinary,             [Allowed,      Allowed,      Forbidden,    Forbidden]),
        spec(K::RosterRoots, "roster.roots", V::SkillRoots, S::SkillRootAccess,                              [Allowed,      Allowed,      Forbidden,    Forbidden]),
        spec(K::PrivacyRedaction, "privacy.redaction", V::Reserved, S::Reserved,                             RESERVED),
        spec(K::PrivacyRawRetention, "privacy.raw_retention", V::Reserved, S::Reserved,                      RESERVED),
    ]
};

/// Ordered by advice authority: `Shadow < Advisory`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum HookMode {
    Shadow,
    Advisory,
}

impl HookMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Shadow => "shadow",
            Self::Advisory => "advisory",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "shadow" => Some(Self::Shadow),
            "advisory" => Some(Self::Advisory),
            _ => None,
        }
    }
}

/// A requested model alias; not an immutable revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelName(String);

impl ModelName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// An unvalidated endpoint override. Origin canonicalization and HTTPS rules
/// belong to the transport boundary; it may contain secret-bearing components.
#[derive(Clone, Eq, PartialEq)]
pub struct EndpointOverride(String);

impl EndpointOverride {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for EndpointOverride {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("EndpointOverride(<private>)")
    }
}

/// An unresolved skill reference; roster resolution decides what it names.
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct SkillReference(String);

impl SkillReference {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SkillReference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SkillReference(<private>)")
    }
}

/// A decoded configuration value before schema validation.
#[derive(Clone, PartialEq)]
pub enum RawValue {
    Bool(bool),
    Integer(i64),
    Float(f64),
    String(String),
    StringList(Vec<String>),
}

impl RawValue {
    const fn type_name(&self) -> &'static str {
        match self {
            Self::Bool(_) => "bool",
            Self::Integer(_) => "integer",
            Self::Float(_) => "float",
            Self::String(_) => "string",
            Self::StringList(_) => "string-list",
        }
    }
}

impl fmt::Debug for RawValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RawValue::{}(<private>)", self.type_name())
    }
}

/// Bounded inputs for one resolution. File entries use dotted paths and come
/// from strict duplicate-rejecting decoders; the environment is a snapshot.
#[derive(Clone, Default)]
pub struct ConfigSources {
    pub trusted_user: Vec<(String, RawValue)>,
    pub project: Vec<(String, RawValue)>,
    pub environment: Vec<(OsString, OsString)>,
    pub cli: Vec<(String, RawValue)>,
}

impl fmt::Debug for ConfigSources {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfigSources")
            .field("trusted_user_entries", &self.trusted_user.len())
            .field("project_entries", &self.project.len())
            .field("environment_variables", &self.environment.len())
            .field("cli_entries", &self.cli.len())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigProblem {
    UnknownKey,
    DuplicateKey,
    TooManyEntries,
    ForbiddenInLayer,
    ReservedSetting,
    WrongType,
    NonFinite,
    OutOfRange,
    InvalidValue,
    ValueTooLong,
    NonUtf8,
    TooManyItems,
    DuplicateItem,
    InvalidRoot(RootError),
    WidensLowerLayer,
    Conflict(SettingKey),
    NotManagedPolicy,
}

impl fmt::Display for ConfigProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownKey => f.write_str("unknown setting"),
            Self::DuplicateKey => f.write_str("setting is defined more than once"),
            Self::TooManyEntries => f.write_str("layer has too many settings"),
            Self::ForbiddenInLayer => f.write_str("setting is not permitted in this layer"),
            Self::ReservedSetting => f.write_str("setting is reserved and not configurable"),
            Self::WrongType => f.write_str("value has the wrong type"),
            Self::NonFinite => f.write_str("value must be finite"),
            Self::OutOfRange => f.write_str("value is outside its bounds"),
            Self::InvalidValue => f.write_str("value is not valid for this setting"),
            Self::ValueTooLong => f.write_str("value exceeds its length limit"),
            Self::NonUtf8 => f.write_str("name or value is not valid UTF-8"),
            Self::TooManyItems => f.write_str("list has too many items"),
            Self::DuplicateItem => f.write_str("list contains a duplicate item"),
            Self::InvalidRoot(err) => write!(f, "invalid root: {err}"),
            Self::WidensLowerLayer => {
                f.write_str("value widens a more restrictive trusted setting")
            }
            Self::Conflict(other) => write!(f, "value conflicts with {}", other.path()),
            Self::NotManagedPolicy => f.write_str("setting is not a managed ranking-policy field"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IssueKey {
    Layer,
    Known(SettingKey),
    /// Printable only when short and ASCII-graphic; values are never included.
    Unknown(Option<String>),
}

impl IssueKey {
    fn unknown(name: &str) -> Self {
        let printable = name.len() <= 64 && name.bytes().all(|b| b.is_ascii_graphic());
        Self::Unknown(printable.then(|| name.to_owned()))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigIssue {
    pub layer: ConfigLayer,
    pub key: IssueKey,
    pub problem: ConfigProblem,
}

impl fmt::Display for ConfigIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: ", self.layer.as_str())?;
        match &self.key {
            IssueKey::Layer => {}
            IssueKey::Known(key) => write!(f, "{}: ", key.path())?,
            IssueKey::Unknown(Some(name)) => write!(f, "{name}: ")?,
            IssueKey::Unknown(None) => f.write_str("<unprintable name>: ")?,
        }
        write!(f, "{}", self.problem)
    }
}

/// All issues found in one pass; any issue invalidates the whole policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigErrors {
    issues: Vec<ConfigIssue>,
    omitted: usize,
}

impl ConfigErrors {
    /// Every configuration issue maps to `invalid-configuration` (exit 2).
    pub const fn kind(&self) -> ErrorKind {
        ErrorKind::InvalidConfiguration
    }

    pub fn issues(&self) -> &[ConfigIssue] {
        &self.issues
    }

    pub const fn omitted(&self) -> usize {
        self.omitted
    }

    pub fn contains(&self, layer: ConfigLayer, key: &IssueKey, problem: ConfigProblem) -> bool {
        self.issues
            .iter()
            .any(|i| i.layer == layer && &i.key == key && i.problem == problem)
    }
}

impl fmt::Display for ConfigErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, issue) in self.issues.iter().enumerate() {
            if index > 0 {
                f.write_str("; ")?;
            }
            write!(f, "{issue}")?;
        }
        if self.omitted > 0 {
            write!(f, "; {} more", self.omitted)?;
        }
        Ok(())
    }
}

impl std::error::Error for ConfigErrors {}

#[derive(Default)]
struct IssueSink {
    issues: Vec<ConfigIssue>,
    omitted: usize,
}

impl IssueSink {
    fn push(&mut self, layer: ConfigLayer, key: IssueKey, problem: ConfigProblem) {
        if self.issues.len() < MAX_REPORTED_ISSUES {
            self.issues.push(ConfigIssue {
                layer,
                key,
                problem,
            });
        } else {
            self.omitted = self.omitted.saturating_add(1);
        }
    }

    fn finish<T>(self, value: T) -> Result<T, ConfigErrors> {
        if self.issues.is_empty() {
            Ok(value)
        } else {
            Err(ConfigErrors {
                issues: self.issues,
                omitted: self.omitted,
            })
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum TypedValue {
    Bool(bool),
    Count(u32),
    Millis(u64),
    Unit(f64),
    Profile(ContextProfile),
    Hook(HookMode),
    Model(ModelName),
    Endpoint(EndpointOverride),
    References(Vec<SkillReference>),
    AbsoluteRoots(Vec<TrustedAbsoluteRoot>),
    Roots(Vec<SkillRoot>),
}

fn bounded_text(text: String, max: usize) -> Result<String, ConfigProblem> {
    if text.is_empty() {
        return Err(ConfigProblem::InvalidValue);
    }
    if text.len() > max {
        return Err(ConfigProblem::ValueTooLong);
    }
    if text.chars().any(forbidden_identity_character) {
        return Err(ConfigProblem::InvalidValue);
    }
    Ok(text)
}

fn bounded_list(items: Vec<String>, max: usize) -> Result<Vec<String>, ConfigProblem> {
    if items.len() > max {
        return Err(ConfigProblem::TooManyItems);
    }
    Ok(items)
}

fn no_duplicates<T: Ord>(items: &[T]) -> Result<(), ConfigProblem> {
    let mut seen = BTreeSet::new();
    if items.iter().all(|item| seen.insert(item)) {
        Ok(())
    } else {
        Err(ConfigProblem::DuplicateItem)
    }
}

fn typed_value(
    spec: &KeySpec,
    layer: ConfigLayer,
    raw: RawValue,
) -> Result<TypedValue, ConfigProblem> {
    use ConfigProblem::{InvalidRoot, InvalidValue, NonFinite, OutOfRange, WrongType};
    match (spec.kind, raw) {
        (ValueKind::Bool, RawValue::Bool(value)) => Ok(TypedValue::Bool(value)),
        (ValueKind::Count { min, max }, RawValue::Integer(value)) => u32::try_from(value)
            .ok()
            .filter(|v| (min..=max).contains(v))
            .map(TypedValue::Count)
            .ok_or(OutOfRange),
        (ValueKind::Millis { min, max }, RawValue::Integer(value)) => u64::try_from(value)
            .ok()
            .filter(|v| (min..=max).contains(v))
            .map(TypedValue::Millis)
            .ok_or(OutOfRange),
        (ValueKind::Unit { min, max }, raw @ (RawValue::Float(_) | RawValue::Integer(_))) => {
            let value = match raw {
                RawValue::Float(value) => value,
                // Integers are exact within every unit bound.
                RawValue::Integer(value) if value.unsigned_abs() <= 1 << 53 => value as f64,
                _ => return Err(OutOfRange),
            };
            if !value.is_finite() {
                Err(NonFinite)
            } else if value < min || value > max {
                Err(OutOfRange)
            } else {
                Ok(TypedValue::Unit(value))
            }
        }
        (ValueKind::ContextProfile, RawValue::String(text)) => ContextProfile::parse(&text)
            .map(TypedValue::Profile)
            .ok_or(InvalidValue),
        (ValueKind::HookMode, RawValue::String(text)) => HookMode::parse(&text)
            .map(TypedValue::Hook)
            .ok_or(InvalidValue),
        (ValueKind::ModelName, RawValue::String(text)) => {
            bounded_text(text, MAX_MODEL_BYTES).map(|t| TypedValue::Model(ModelName(t)))
        }
        (ValueKind::Endpoint, RawValue::String(text)) => bounded_text(text, MAX_STRING_VALUE_BYTES)
            .map(|t| TypedValue::Endpoint(EndpointOverride(t))),
        (ValueKind::SkillReferences, RawValue::StringList(items)) => {
            let refs = bounded_list(items, MAX_LIST_ITEMS)?
                .into_iter()
                .map(|item| bounded_text(item, MAX_ID_BYTES).map(SkillReference))
                .collect::<Result<Vec<_>, _>>()?;
            no_duplicates(&refs)?;
            Ok(TypedValue::References(refs))
        }
        (ValueKind::TrustedAbsoluteRoots, RawValue::StringList(items)) => {
            let roots = bounded_list(items, MAX_ROOTS)?
                .iter()
                .map(|item| TrustedAbsoluteRoot::parse(item).map_err(InvalidRoot))
                .collect::<Result<Vec<_>, _>>()?;
            no_duplicates(&roots)?;
            Ok(TypedValue::AbsoluteRoots(roots))
        }
        (ValueKind::SkillRoots, RawValue::StringList(items)) => {
            let roots = bounded_list(items, MAX_ROOTS)?
                .iter()
                .map(|item| skill_root(layer, item).map_err(InvalidRoot))
                .collect::<Result<Vec<_>, _>>()?;
            no_duplicates(&roots)?;
            Ok(TypedValue::Roots(roots))
        }
        (ValueKind::Credential | ValueKind::Reserved, _) => Err(InvalidValue),
        _ => Err(WrongType),
    }
}

/// Only trusted user configuration may grant a root outside the workspace.
fn skill_root(layer: ConfigLayer, text: &str) -> Result<SkillRoot, RootError> {
    if layer == ConfigLayer::TrustedUser && std::path::Path::new(text).is_absolute() {
        TrustedAbsoluteRoot::parse(text).map(SkillRoot::TrustedAbsolute)
    } else {
        WorkspaceRelativeRoot::parse(text).map(SkillRoot::WorkspaceRelative)
    }
}

/// Environment values are text; parse them into the same raw forms as files.
fn environment_raw(kind: ValueKind, text: String) -> Result<RawValue, ConfigProblem> {
    match kind {
        ValueKind::Bool => match text.as_str() {
            "true" => Ok(RawValue::Bool(true)),
            "false" => Ok(RawValue::Bool(false)),
            _ => Err(ConfigProblem::WrongType),
        },
        ValueKind::Count { .. } | ValueKind::Millis { .. } => {
            if text.is_empty() || text.len() > 19 || !text.bytes().all(|b| b.is_ascii_digit()) {
                return Err(ConfigProblem::WrongType);
            }
            text.parse()
                .map(RawValue::Integer)
                .map_err(|_| ConfigProblem::OutOfRange)
        }
        ValueKind::Unit { .. } => {
            let value: f64 = text.parse().map_err(|_| ConfigProblem::WrongType)?;
            Ok(RawValue::Float(value))
        }
        _ => Ok(RawValue::String(text)),
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct ValidatedLayer {
    values: BTreeMap<SettingKey, TypedValue>,
}

fn check_rule(spec: &KeySpec, layer: ConfigLayer, sink: &mut IssueSink) -> bool {
    if spec.rule(layer) != Forbidden {
        return true;
    }
    let problem = if spec.sensitivity == Sensitivity::Reserved {
        ConfigProblem::ReservedSetting
    } else {
        ConfigProblem::ForbiddenInLayer
    };
    sink.push(layer, IssueKey::Known(spec.key), problem);
    false
}

fn validate_entries(
    layer: ConfigLayer,
    entries: Vec<(String, RawValue)>,
    sink: &mut IssueSink,
) -> ValidatedLayer {
    let mut builder = LayerBuilder::new(layer);
    if entries.len() > MAX_LAYER_ENTRIES {
        sink.push(layer, IssueKey::Layer, ConfigProblem::TooManyEntries);
        return builder.validated;
    }
    for (name, raw) in entries {
        let key = (name.len() <= MAX_KEY_BYTES)
            .then(|| SettingKey::from_path(&name))
            .flatten();
        match key {
            Some(key) => builder.insert(key, Ok(raw), sink),
            None => sink.push(layer, IssueKey::unknown(&name), ConfigProblem::UnknownKey),
        }
    }
    builder.validated
}

/// Tracks every key seen, so a repeated key is reported even when its first
/// occurrence was itself invalid or forbidden.
struct LayerBuilder {
    layer: ConfigLayer,
    seen: BTreeSet<SettingKey>,
    validated: ValidatedLayer,
}

impl LayerBuilder {
    fn new(layer: ConfigLayer) -> Self {
        Self {
            layer,
            seen: BTreeSet::new(),
            validated: ValidatedLayer::default(),
        }
    }

    /// Returns false for a duplicate; other problems are reported to `sink`.
    fn first_occurrence(&mut self, key: SettingKey, sink: &mut IssueSink) -> bool {
        if self.seen.insert(key) {
            return true;
        }
        sink.push(
            self.layer,
            IssueKey::Known(key),
            ConfigProblem::DuplicateKey,
        );
        false
    }

    fn insert(
        &mut self,
        key: SettingKey,
        raw: Result<RawValue, ConfigProblem>,
        sink: &mut IssueSink,
    ) {
        let spec = key.spec();
        if !self.first_occurrence(key, sink) || !check_rule(spec, self.layer, sink) {
            return;
        }
        match raw.and_then(|raw| typed_value(spec, self.layer, raw)) {
            Ok(value) => {
                self.validated.values.insert(key, value);
            }
            Err(problem) => sink.push(self.layer, IssueKey::Known(key), problem),
        }
    }
}

/// Strict for the `SR_` namespace: an unrecognized `SR_*` variable is an error
/// so a misspelled privacy setting cannot be silently ignored. Other variables,
/// including unrelated `TYPESAFE_*` names, are outside this schema.
fn validate_environment(
    vars: Vec<(OsString, OsString)>,
    sink: &mut IssueSink,
) -> (ValidatedLayer, Option<ApiCredential>) {
    let layer = ConfigLayer::Environment;
    let mut builder = LayerBuilder::new(layer);
    let mut credential = None;
    let mut recognized = 0usize;
    for (name, value) in vars {
        let strict_namespace = name.as_encoded_bytes().starts_with(b"SR_");
        let Some(name) = name.to_str() else {
            if strict_namespace {
                sink.push(layer, IssueKey::Unknown(None), ConfigProblem::NonUtf8);
            }
            continue;
        };
        let Some(key) = SettingKey::from_environment_name(name) else {
            if strict_namespace {
                sink.push(layer, IssueKey::unknown(name), ConfigProblem::UnknownKey);
            }
            continue;
        };
        recognized += 1;
        if recognized > MAX_LAYER_ENTRIES {
            sink.push(layer, IssueKey::Layer, ConfigProblem::TooManyEntries);
            break;
        }
        let text = value.into_string().map_err(|_| ConfigProblem::NonUtf8);
        // The credential never enters a validated layer, receipt or merge.
        if key == SettingKey::TypesafeApiKey {
            if !builder.first_occurrence(key, sink) {
                continue;
            }
            match text.and_then(|t| {
                ApiCredential::from_environment(t).map_err(|err| match err {
                    CredentialError::TooLong => ConfigProblem::ValueTooLong,
                    CredentialError::InvalidCharacter => ConfigProblem::InvalidValue,
                })
            }) {
                Ok(found) => credential = found,
                Err(problem) => sink.push(layer, IssueKey::Known(key), problem),
            }
            continue;
        }
        let kind = key.spec().kind;
        builder.insert(key, text.and_then(|t| environment_raw(kind, t)), sink);
    }
    (builder.validated, credential)
}

/// Where an effective value came from. Union lists record every contributor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ValueSource {
    Single(ConfigLayer),
    Union(BTreeSet<ConfigLayer>),
}

/// Effective ordinary and authority values. Credentials are held separately.
#[derive(Clone, Debug, PartialEq)]
pub struct EffectiveConfig {
    network_enabled: bool,
    endpoint: Option<EndpointOverride>,
    model: ModelName,
    hook_mode: HookMode,
    context_profile: ContextProfile,
    no_tools: bool,
    messages: u32,
    budget_chars: u32,
    transcript_roots: Vec<TrustedAbsoluteRoot>,
    top: u32,
    shortlist: u32,
    gate: f64,
    fits: f64,
    w_fit: f64,
    w_prior: f64,
    w_phase: f64,
    timeout_ms: u64,
    exclude_skills: Vec<SkillReference>,
    roster_roots: Vec<SkillRoot>,
}

impl EffectiveConfig {
    pub fn defaults() -> Self {
        Self {
            network_enabled: false,
            endpoint: None,
            model: ModelName(DEFAULT_MODEL.to_owned()),
            hook_mode: HookMode::Shadow,
            context_profile: ContextProfile::Standard,
            no_tools: false,
            messages: RECENT_NORMALIZED_MESSAGES.max() as u32,
            budget_chars: RENDERED_CONTEXT_SCALARS.max() as u32,
            transcript_roots: Vec::new(),
            top: 5,
            shortlist: 8,
            gate: 0.30,
            fits: 0.30,
            w_fit: 1.0,
            w_prior: 0.0,
            w_phase: 0.0,
            timeout_ms: DEFAULT_INVOCATION_DEADLINE_MS,
            exclude_skills: Vec::new(),
            roster_roots: Vec::new(),
        }
    }

    /// The trusted user's setting only; use `ResolvedConfig::network_consent`.
    pub const fn trusted_user_network_enabled(&self) -> bool {
        self.network_enabled
    }
    pub fn endpoint(&self) -> Option<&EndpointOverride> {
        self.endpoint.as_ref()
    }
    pub fn model(&self) -> &ModelName {
        &self.model
    }
    pub const fn hook_mode(&self) -> HookMode {
        self.hook_mode
    }
    pub const fn context_profile(&self) -> ContextProfile {
        self.context_profile
    }
    pub const fn no_tools(&self) -> bool {
        self.no_tools
    }
    pub const fn messages(&self) -> u32 {
        self.messages
    }
    pub const fn budget_chars(&self) -> u32 {
        self.budget_chars
    }
    pub fn transcript_roots(&self) -> &[TrustedAbsoluteRoot] {
        &self.transcript_roots
    }
    pub const fn top(&self) -> u32 {
        self.top
    }
    pub const fn shortlist(&self) -> u32 {
        self.shortlist
    }
    pub const fn gate(&self) -> f64 {
        self.gate
    }
    pub const fn fits(&self) -> f64 {
        self.fits
    }
    pub const fn w_fit(&self) -> f64 {
        self.w_fit
    }
    pub const fn w_prior(&self) -> f64 {
        self.w_prior
    }
    pub const fn w_phase(&self) -> f64 {
        self.w_phase
    }
    pub const fn timeout_ms(&self) -> u64 {
        self.timeout_ms
    }
    pub fn exclude_skills(&self) -> &[SkillReference] {
        &self.exclude_skills
    }
    pub fn roster_roots(&self) -> &[SkillRoot] {
        &self.roster_roots
    }

    /// True when `value` would allow more disclosure or authority than the
    /// current value. Only keys with a `RestrictOnly` layer are ordered.
    fn widens(&self, key: SettingKey, value: &TypedValue) -> bool {
        match (key, value) {
            (SettingKey::HookMode, TypedValue::Hook(mode)) => *mode > self.hook_mode,
            (SettingKey::ContextProfile, TypedValue::Profile(p)) => *p > self.context_profile,
            (SettingKey::ContextNoTools, TypedValue::Bool(no_tools)) => self.no_tools && !no_tools,
            (SettingKey::ContextMessages, TypedValue::Count(n)) => *n > self.messages,
            (SettingKey::ContextBudgetChars, TypedValue::Count(n)) => *n > self.budget_chars,
            // An unordered key under RestrictOnly fails closed.
            _ => true,
        }
    }

    fn apply(&mut self, key: SettingKey, value: TypedValue) {
        use SettingKey as K;
        use TypedValue as T;
        match (key, value) {
            (K::NetworkEnabled, T::Bool(v)) => self.network_enabled = v,
            (K::TypesafeEndpoint, T::Endpoint(v)) => self.endpoint = Some(v),
            (K::ProviderModel, T::Model(v)) => self.model = v,
            (K::HookMode, T::Hook(v)) => self.hook_mode = v,
            (K::ContextProfile, T::Profile(v)) => self.context_profile = v,
            (K::ContextNoTools, T::Bool(v)) => self.no_tools = v,
            (K::ContextMessages, T::Count(v)) => self.messages = v,
            (K::ContextBudgetChars, T::Count(v)) => self.budget_chars = v,
            (K::ContextTranscriptRoots, T::AbsoluteRoots(v)) => self.transcript_roots = v,
            (K::RankingTop, T::Count(v)) => self.top = v,
            (K::RankingShortlist, T::Count(v)) => self.shortlist = v,
            (K::RankingGate, T::Unit(v)) => self.gate = v,
            (K::RankingFits, T::Unit(v)) => self.fits = v,
            (K::RankingWFit, T::Unit(v)) => self.w_fit = v,
            (K::RankingWPrior, T::Unit(v)) => self.w_prior = v,
            (K::RankingWPhase, T::Unit(v)) => self.w_phase = v,
            (K::RankingTimeoutMs, T::Millis(v)) => self.timeout_ms = v,
            (K::RankingExcludeSkills, T::References(v)) => union_into(&mut self.exclude_skills, v),
            (K::RosterRoots, T::Roots(v)) => union_into(&mut self.roster_roots, v),
            (key, _) => unreachable!("validated value kind for {}", key.path()),
        }
    }
}

/// Later layers can add to a union list but never remove earlier items.
fn union_into<T: Ord + Clone>(existing: &mut Vec<T>, added: Vec<T>) {
    let seen: BTreeSet<T> = existing.iter().cloned().collect();
    existing.extend(added.into_iter().filter(|item| !seen.contains(item)));
}

/// A validated configuration snapshot for one invocation.
#[derive(Clone, Debug)]
pub struct ResolvedConfig {
    effective: EffectiveConfig,
    sources: BTreeMap<SettingKey, ValueSource>,
    credential: Option<ApiCredential>,
    /// Process environment and arguments stay fixed for the whole invocation.
    environment: ValidatedLayer,
    cli: ValidatedLayer,
    generation: u64,
}

impl ResolvedConfig {
    /// `generation` identifies this read sequence in receipts.
    pub fn resolve(sources: ConfigSources, generation: u64) -> Result<Self, ConfigErrors> {
        let mut sink = IssueSink::default();
        let user = validate_entries(ConfigLayer::TrustedUser, sources.trusted_user, &mut sink);
        let project = validate_entries(ConfigLayer::Project, sources.project, &mut sink);
        let (environment, credential) = validate_environment(sources.environment, &mut sink);
        let cli = validate_entries(ConfigLayer::Cli, sources.cli, &mut sink);
        let merged = merge(&user, &project, &environment, &cli, &mut sink);
        sink.finish(merged).map(|(effective, sources)| Self {
            effective,
            sources,
            credential,
            environment,
            cli,
            generation,
        })
    }

    /// Re-reads only the mutable file layers through the same trust rules,
    /// keeping this invocation's environment and command-line layers.
    pub fn reresolve_files(
        &self,
        trusted_user: Vec<(String, RawValue)>,
        project: Vec<(String, RawValue)>,
        generation: u64,
    ) -> Result<Self, ConfigErrors> {
        let mut sink = IssueSink::default();
        let user = validate_entries(ConfigLayer::TrustedUser, trusted_user, &mut sink);
        let project = validate_entries(ConfigLayer::Project, project, &mut sink);
        let merged = merge(&user, &project, &self.environment, &self.cli, &mut sink);
        sink.finish(merged).map(|(effective, sources)| Self {
            effective,
            sources,
            credential: self.credential.clone(),
            environment: self.environment.clone(),
            cli: self.cli.clone(),
            generation,
        })
    }

    pub fn effective(&self) -> &EffectiveConfig {
        &self.effective
    }

    pub fn source(&self, key: SettingKey) -> ValueSource {
        self.sources
            .get(&key)
            .cloned()
            .unwrap_or(ValueSource::Single(ConfigLayer::BuiltIn))
    }

    pub fn credential(&self) -> Option<&ApiCredential> {
        self.credential.as_ref()
    }

    pub const fn credential_status(&self) -> CredentialStatus {
        if self.credential.is_some() {
            CredentialStatus::PresentFromEnvironment
        } else {
            CredentialStatus::Absent
        }
    }

    pub const fn network_consent(&self, effects: EffectPolicy) -> NetworkConsent {
        NetworkConsent::derive(effects, self.effective.network_enabled)
    }

    pub fn receipt(&self, effects: EffectPolicy) -> PolicyReceipt {
        PolicyReceipt {
            schema_version: CONFIG_SCHEMA_VERSION,
            generation: self.generation,
            effects,
            network_consent: self.network_consent(effects),
            credential: self.credential_status(),
            effective: self.effective.clone(),
            sources: self.sources.clone(),
        }
    }

    /// Re-resolves mutable configuration and compares the fields `boundary`
    /// depends on. Invalid current configuration fails closed. A change only
    /// withholds; it never authorizes a replacement request.
    pub fn revalidate(
        &self,
        receipt: &PolicyReceipt,
        trusted_user: Vec<(String, RawValue)>,
        project: Vec<(String, RawValue)>,
        generation: u64,
        boundary: PolicyBoundary,
    ) -> Revalidation {
        match self.reresolve_files(trusted_user, project, generation) {
            Ok(current) => receipt.compare(&current.receipt(receipt.effects), boundary),
            Err(_) => Revalidation::InvalidConfiguration,
        }
    }
}

fn merge(
    user: &ValidatedLayer,
    project: &ValidatedLayer,
    environment: &ValidatedLayer,
    cli: &ValidatedLayer,
    sink: &mut IssueSink,
) -> (EffectiveConfig, BTreeMap<SettingKey, ValueSource>) {
    let mut effective = EffectiveConfig::defaults();
    let mut sources = BTreeMap::new();
    let layers = [
        (ConfigLayer::TrustedUser, user),
        (ConfigLayer::Project, project),
        (ConfigLayer::Environment, environment),
        (ConfigLayer::Cli, cli),
    ];
    let mut applied: BTreeMap<SettingKey, ConfigLayer> = BTreeMap::new();
    for (layer, validated) in layers {
        for (&key, value) in &validated.values {
            let spec = key.spec();
            if spec.rule(layer) == RestrictOnly && effective.widens(key, value) {
                sink.push(layer, IssueKey::Known(key), ConfigProblem::WidensLowerLayer);
                continue;
            }
            let source = if spec.kind.merges_as_union() {
                let mut layers = match sources.remove(&key) {
                    Some(ValueSource::Union(layers)) => layers,
                    _ => BTreeSet::new(),
                };
                layers.insert(layer);
                ValueSource::Union(layers)
            } else {
                ValueSource::Single(layer)
            };
            effective.apply(key, value.clone());
            sources.insert(key, source);
            applied.insert(key, layer);
        }
    }
    if effective.top > effective.shortlist {
        let top_layer = applied.get(&SettingKey::RankingTop).copied();
        let shortlist_layer = applied.get(&SettingKey::RankingShortlist).copied();
        let layer = top_layer
            .max(shortlist_layer)
            .unwrap_or(ConfigLayer::BuiltIn);
        sink.push(
            layer,
            IssueKey::Known(SettingKey::RankingTop),
            ConfigProblem::Conflict(SettingKey::RankingShortlist),
        );
    }
    (effective, sources)
}

/// Fields an invocation's work was conditioned on.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum PolicyField {
    NetworkConsent,
    CredentialPresence,
    Endpoint,
    Model,
    ContextProfile,
    NoTools,
    Messages,
    BudgetChars,
    TranscriptRoots,
    Top,
    Shortlist,
    Gate,
    Fits,
    WFit,
    WPrior,
    WPhase,
    ExcludeSkills,
    RosterRoots,
    HookMode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublicationKind {
    Advisory,
    Explicit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PolicyBoundary {
    /// Before each new HTTP attempt, including a retry.
    ProviderAdmission,
    CliPublication(PublicationKind),
    HookPublication(PublicationKind),
}

impl PolicyBoundary {
    /// The dependency projection. The invocation deadline is fixed at entry
    /// and appears in none of them.
    pub const fn dependencies(self) -> &'static [PolicyField] {
        use PolicyField as F;
        const ADMISSION: &[PolicyField] = &[
            F::NetworkConsent,
            F::CredentialPresence,
            F::Endpoint,
            F::Model,
            F::ContextProfile,
            F::NoTools,
            F::Messages,
            F::BudgetChars,
            F::TranscriptRoots,
            F::Shortlist,
            F::ExcludeSkills,
            F::RosterRoots,
        ];
        const ADVISORY: &[PolicyField] = &[
            F::Top,
            F::Shortlist,
            F::Gate,
            F::Fits,
            F::WFit,
            F::WPrior,
            F::WPhase,
            F::ExcludeSkills,
            F::RosterRoots,
        ];
        const HOOK_ADVISORY: &[PolicyField] = &[
            F::Top,
            F::Shortlist,
            F::Gate,
            F::Fits,
            F::WFit,
            F::WPrior,
            F::WPhase,
            F::ExcludeSkills,
            F::RosterRoots,
            F::HookMode,
        ];
        match self {
            Self::ProviderAdmission => ADMISSION,
            Self::CliPublication(PublicationKind::Advisory) => ADVISORY,
            Self::CliPublication(PublicationKind::Explicit) => &[F::RosterRoots],
            Self::HookPublication(PublicationKind::Advisory) => HOOK_ADVISORY,
            Self::HookPublication(PublicationKind::Explicit) => &[F::RosterRoots, F::HookMode],
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Revalidation {
    Unchanged,
    Superseded(Vec<PolicyField>),
    InvalidConfiguration,
}

/// The effective policy an invocation used, with category-level provenance.
/// It holds no credential value; its Debug output hides paths and endpoints.
#[derive(Clone, Debug, PartialEq)]
pub struct PolicyReceipt {
    schema_version: u32,
    generation: u64,
    effects: EffectPolicy,
    network_consent: NetworkConsent,
    credential: CredentialStatus,
    effective: EffectiveConfig,
    sources: BTreeMap<SettingKey, ValueSource>,
}

impl PolicyReceipt {
    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }
    pub const fn generation(&self) -> u64 {
        self.generation
    }
    pub const fn effects(&self) -> EffectPolicy {
        self.effects
    }
    pub const fn network_consent(&self) -> NetworkConsent {
        self.network_consent
    }
    pub const fn credential(&self) -> CredentialStatus {
        self.credential
    }
    pub fn effective(&self) -> &EffectiveConfig {
        &self.effective
    }
    pub fn source(&self, key: SettingKey) -> ValueSource {
        self.sources
            .get(&key)
            .cloned()
            .unwrap_or(ValueSource::Single(ConfigLayer::BuiltIn))
    }

    /// Compares effective values, not sources, generations or file bytes.
    pub fn compare(&self, current: &PolicyReceipt, boundary: PolicyBoundary) -> Revalidation {
        let (a, b) = (&self.effective, &current.effective);
        let changed: Vec<PolicyField> = boundary
            .dependencies()
            .iter()
            .copied()
            .filter(|field| match field {
                PolicyField::NetworkConsent => self.network_consent != current.network_consent,
                PolicyField::CredentialPresence => self.credential != current.credential,
                PolicyField::Endpoint => a.endpoint != b.endpoint,
                PolicyField::Model => a.model != b.model,
                PolicyField::ContextProfile => a.context_profile != b.context_profile,
                PolicyField::NoTools => a.no_tools != b.no_tools,
                PolicyField::Messages => a.messages != b.messages,
                PolicyField::BudgetChars => a.budget_chars != b.budget_chars,
                PolicyField::TranscriptRoots => a.transcript_roots != b.transcript_roots,
                PolicyField::Top => a.top != b.top,
                PolicyField::Shortlist => a.shortlist != b.shortlist,
                PolicyField::Gate => a.gate != b.gate,
                PolicyField::Fits => a.fits != b.fits,
                PolicyField::WFit => a.w_fit != b.w_fit,
                PolicyField::WPrior => a.w_prior != b.w_prior,
                PolicyField::WPhase => a.w_phase != b.w_phase,
                PolicyField::ExcludeSkills => a.exclude_skills != b.exclude_skills,
                PolicyField::RosterRoots => a.roster_roots != b.roster_roots,
                PolicyField::HookMode => a.hook_mode != b.hook_mode,
            })
            .collect();
        if changed.is_empty() {
            Revalidation::Unchanged
        } else {
            Revalidation::Superseded(changed)
        }
    }
}

/// A calibration apply or rollback against trusted user configuration. The
/// writer previews first, compares `base` to the file's current digest before
/// an atomic replace, keeps a private backup and preserves every other setting.
/// No variant can target project configuration or authority settings.
#[derive(Clone, Debug, PartialEq)]
pub struct ManagedPolicyMutation {
    base: ContentHash,
    changes: BTreeMap<SettingKey, TypedValue>,
}

impl ManagedPolicyMutation {
    pub fn new(base: ContentHash, changes: Vec<(String, RawValue)>) -> Result<Self, ConfigErrors> {
        let layer = ConfigLayer::TrustedUser;
        let mut sink = IssueSink::default();
        let mut builder = LayerBuilder::new(layer);
        for (name, raw) in changes {
            match SettingKey::from_path(&name) {
                Some(key) if key.spec().managed_policy => builder.insert(key, Ok(raw), &mut sink),
                Some(key) => {
                    sink.push(layer, IssueKey::Known(key), ConfigProblem::NotManagedPolicy)
                }
                None => sink.push(layer, IssueKey::unknown(&name), ConfigProblem::UnknownKey),
            }
        }
        sink.finish(Self {
            base,
            changes: builder.validated.values,
        })
    }

    pub fn base_digest(&self) -> &ContentHash {
        &self.base
    }

    pub fn keys(&self) -> impl Iterator<Item = SettingKey> + '_ {
        self.changes.keys().copied()
    }
}
