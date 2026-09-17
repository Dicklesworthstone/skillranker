//! Local roster identity and visibility contracts. Discovery and validation of
//! actual files/invocation rules belong to the selected harness adapter.

pub mod frontmatter;
pub mod retrieval;

pub use frontmatter::{
    BODY_EXCERPT_MAX_SCALARS, FrontmatterError, MAX_FRONTMATTER_BYTES, MAX_SKILL_FILE_BYTES,
    ParsedSkillMetadata, RERANK_DESCRIPTION_MAX_SCALARS, WIDE_DESCRIPTION_MAX_SCALARS,
    parse_skill_metadata,
};

use crate::context::PrivateText;
use crate::identity::{ContentHash, IdentityError, OpaqueLoadTarget, SkillId, SourceId};
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};

/// A callable identifier, distinct from a title or internal source-qualified ID.
/// Passing these lexical checks does not prove a harness accepts the name.
#[derive(Clone, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct InvocationName(String);

impl fmt::Debug for InvocationName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("InvocationName(<private>)")
    }
}

impl InvocationName {
    pub fn new(name: impl Into<String>) -> Result<Self, IdentityError> {
        let name = name.into();
        if name.is_empty() {
            return Err(IdentityError::Empty);
        }
        if name.len() > crate::identity::MAX_ID_BYTES {
            return Err(IdentityError::TooLong);
        }
        if name
            .chars()
            .any(crate::identity::forbidden_identity_character)
        {
            return Err(IdentityError::InvalidCharacter);
        }
        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for InvocationName {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct DisplayName(String);

impl fmt::Debug for DisplayName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DisplayName(<private>)")
    }
}

impl DisplayName {
    /// Preserve readable text while stripping terminal controls and directional
    /// overrides. Rendering still applies its own display-cell/output budgets.
    pub fn from_text(text: &str) -> Self {
        Self(text.chars().filter(|c| {
            !c.is_control() && !matches!(*c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        }).collect())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Native path bytes are retained locally; no lossy String conversion or
/// default serialization can turn this into provider text or imported authority.
#[derive(Clone, Eq, PartialEq)]
pub struct LocalPath(PathBuf);

impl LocalPath {
    pub fn new(path: PathBuf) -> Self {
        Self(path)
    }
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

impl fmt::Debug for LocalPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LocalPath(<private>)")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoadTarget {
    File(LocalPath),
    Harness(OpaqueLoadTarget),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageKind {
    Reference,
    Workflow,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Visibility {
    Verified { contract_version: String },
    Shadowed { winner: SkillId },
    Ambiguous,
    Unverified,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct InvocationRestrictions {
    pub agent_invocable: bool,
    pub user_invocable: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InvocationKind {
    Agent,
    ManualOnly,
    Forbidden,
}

impl InvocationRestrictions {
    pub fn explicit_kind(self) -> InvocationKind {
        if self.agent_invocable {
            InvocationKind::Agent
        } else if self.user_invocable {
            InvocationKind::ManualOnly
        } else {
            InvocationKind::Forbidden
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillAlias {
    pub source: SourceId,
    pub invocation: InvocationName,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ParseWarning {
    MissingFrontmatter,
    TruncatedOptionalField,
    UnsupportedOptionalMetadata,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillRecord {
    pub id: SkillId,
    pub source: SourceId,
    pub source_priority: i32,
    pub invocation_name: InvocationName,
    pub display_name: DisplayName,
    pub target: LoadTarget,
    pub source_content: ContentHash,
    pub rendered_content: Option<ContentHash>,
    pub visibility: Visibility,
    pub restrictions: InvocationRestrictions,
    pub usage_kind: UsageKind,
    pub aliases: Vec<SkillAlias>,
    pub description_full: PrivateText,
    pub description_short: PrivateText,
    pub body_excerpt: PrivateText,
    pub tags: Vec<PrivateText>,
    pub phases: Vec<PrivateText>,
    pub parse_warnings: Vec<ParseWarning>,
}
