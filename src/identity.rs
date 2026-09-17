//! Local identity contracts. These values are never provider authority.
//!
//! Adapters resolve filesystem identities and source provenance before creating
//! a session. Unknown attribution deliberately cannot produce a durable key.

use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeSet;
use std::fmt;

/// Bound opaque identifiers independently of the enclosing input's byte limit.
pub const MAX_ID_BYTES: usize = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentityError {
    Empty,
    TooLong,
    InvalidCharacter,
    InvalidDigest,
    ReservedOption,
    DuplicateDefinition,
    UnknownAttribution,
}

impl fmt::Display for IdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "identity is empty",
            Self::TooLong => "identity exceeds its byte limit",
            Self::InvalidCharacter => "identity contains a forbidden character",
            Self::InvalidDigest => "content digest must contain 64 lowercase hexadecimal digits",
            Self::ReservedOption => "option identifier is reserved",
            Self::DuplicateDefinition => "duplicate identity in a definition collection",
            Self::UnknownAttribution => "durable session attribution is incomplete",
        })
    }
}

impl std::error::Error for IdentityError {}

fn validate_id(value: &str) -> Result<(), IdentityError> {
    if value.is_empty() {
        return Err(IdentityError::Empty);
    }
    if value.len() > MAX_ID_BYTES {
        return Err(IdentityError::TooLong);
    }
    if value.chars().any(forbidden_identity_character) {
        return Err(IdentityError::InvalidCharacter);
    }
    Ok(())
}

pub(crate) fn forbidden_identity_character(c: char) -> bool {
    c.is_whitespace()
        || c.is_control()
        || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

macro_rules! identifier {
    ($validator:path; $($name:ident),+ $(,)?) => {$ (
        #[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, IdentityError> {
                let value = value.into();
                $validator(&value)?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                // A session/producer ID can itself contain private information.
                f.write_str(concat!(stringify!($name), "(<local-id>)"))
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                Self::new(String::deserialize(d)?).map_err(serde::de::Error::custom)
            }
        }
    )+};
}

identifier!(validate_id;
    WorkspaceId,
    SessionId,
    AgentId,
    BranchId,
    ContextEpoch,
    AdapterId,
    AdapterVersion,
    ProducerId,
    SourceId,
    EventId,
    TurnId,
    InvocationId,
    LogicalSkillKey,
    HarnessId,
    ToolCallId,
    OpaqueLoadTarget,
);

fn validate_skill_id(value: &str) -> Result<(), IdentityError> {
    validate_id(value)?;
    if value == "__none__" {
        return Err(IdentityError::ReservedOption);
    }
    Ok(())
}

identifier!(validate_skill_id; SkillId);

/// Distinct from skill IDs: this is only a handle inside one provider question.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct OptionId(String);

impl OptionId {
    pub fn new(value: impl Into<String>) -> Result<Self, IdentityError> {
        let value = value.into();
        validate_id(&value)?;
        if value == "__none__" {
            return Err(IdentityError::ReservedOption);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for OptionId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

/// Explicit sentinel type; a skill ID or option cannot impersonate `None`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChoiceTarget {
    None,
    Skill(OptionId),
}

/// A BLAKE3 digest of the exact bytes consumed, not a path or mtime assertion.
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize)]
#[serde(transparent)]
pub struct ContentHash(String);

impl ContentHash {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self(blake3::hash(bytes).to_hex().to_string())
    }

    pub fn parse(value: impl Into<String>) -> Result<Self, IdentityError> {
        let value = value.into();
        if value.len() != 64
            || !value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(IdentityError::InvalidDigest);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ContentHash(<local-digest>)")
    }
}

impl<'de> Deserialize<'de> for ContentHash {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::parse(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

impl SkillId {
    /// Length framing prevents (`ab`, `c`) from aliasing (`a`, `bc`).
    /// Content changes are tracked by ContentHash, independently of stable ID.
    pub fn from_source(source: &SourceId, logical_key: &LogicalSkillKey) -> Self {
        let mut hash = blake3::Hasher::new_derive_key("skillranker.skill-identity.v1");
        for part in [source.as_str(), logical_key.as_str()] {
            hash.update(&(part.len() as u64).to_le_bytes());
            hash.update(part.as_bytes());
        }
        Self(format!("s_{}", hash.finalize().to_hex()))
    }
}

/// Determined by the selected adapter, never by an imported `harness` field.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum SourceProvenance {
    Native {
        adapter: AdapterId,
        version: AdapterVersion,
    },
    Normalized {
        producer: Option<ProducerId>,
        harness: HarnessId,
        schema_version: u32,
    },
    Cass {
        source: Option<SourceId>,
        version: AdapterVersion,
    },
}

impl SourceProvenance {
    fn is_attributable(&self) -> bool {
        match self {
            Self::Native { .. } => true,
            Self::Normalized {
                producer,
                schema_version,
                ..
            } => producer.is_some() && *schema_version != 0,
            Self::Cass { source, .. } => source.is_some(),
        }
    }
}

/// Incomplete fields stay explicit None rather than inventing a shared identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionIdentity {
    pub source: SourceProvenance,
    pub workspace: Option<WorkspaceId>,
    pub session: Option<SessionId>,
    pub agent: Option<AgentId>,
    pub branch: Option<BranchId>,
    pub epoch: Option<ContextEpoch>,
}

/// Construction is restricted to complete attribution. This is local data,
/// deliberately neither Serialize nor Deserialize. Persistence must use an
/// explicit private-store codec; public output must use its own safe receipt.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct DurableSessionNamespace {
    source: SourceProvenance,
    workspace: WorkspaceId,
    session: SessionId,
    agent: AgentId,
    branch: BranchId,
    epoch: ContextEpoch,
}

/// A durable event reference needs both complete session attribution and a
/// concrete event ID. It is attribution only, never proof of observed loading.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct DurableEventKey {
    namespace: DurableSessionNamespace,
    event: EventId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionNamespace {
    Durable {
        namespace: DurableSessionNamespace,
    },
    InvocationLocal {
        invocation: InvocationId,
        identity: SessionIdentity,
    },
}

impl SessionIdentity {
    pub fn event_key(&self, event: Option<&EventId>) -> Result<DurableEventKey, IdentityError> {
        Ok(DurableEventKey {
            namespace: self.durable_namespace()?,
            event: event.ok_or(IdentityError::UnknownAttribution)?.clone(),
        })
    }

    pub fn durable_namespace(&self) -> Result<DurableSessionNamespace, IdentityError> {
        let missing = IdentityError::UnknownAttribution;
        if !self.source.is_attributable() {
            return Err(missing);
        }
        Ok(DurableSessionNamespace {
            source: self.source.clone(),
            workspace: self.workspace.clone().ok_or(missing)?,
            session: self.session.clone().ok_or(missing)?,
            agent: self.agent.clone().ok_or(missing)?,
            branch: self.branch.clone().ok_or(missing)?,
            epoch: self.epoch.clone().ok_or(missing)?,
        })
    }

    /// The entry boundary supplies a fresh invocation nonce. It must not derive
    /// this ID from prompt text, an empty session ID, or wall-clock time alone.
    pub fn namespace(&self, invocation: InvocationId) -> SessionNamespace {
        match self.durable_namespace() {
            Ok(namespace) => SessionNamespace::Durable { namespace },
            Err(_) => SessionNamespace::InvocationLocal {
                invocation,
                identity: self.clone(),
            },
        }
    }
}

/// Call separately for each definition collection. Repeated references across
/// wide/rerank sets are allowed; duplicate definitions within one set are not.
pub fn validate_definition_ids<'a, T: Ord + 'a>(
    ids: impl IntoIterator<Item = &'a T>,
) -> Result<(), IdentityError> {
    let mut seen = BTreeSet::new();
    for id in ids {
        if !seen.insert(id) {
            return Err(IdentityError::DuplicateDefinition);
        }
    }
    Ok(())
}
