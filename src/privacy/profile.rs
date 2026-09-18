//! Trusted standard and minimal disclosure profiles and trust-layer resolution.
//!
//! Enforces boundary `p3_disclosure_profiles`:
//! - Standard profile preserves the default request-first context within bounds.
//! - Minimal profile retains the current request, essential task anchor, explicit constraints,
//!   and per-stage candidate material while omitting optional history, tool bodies, and dirty paths.
//! - `--no-tools` further restricts either profile, guaranteeing zero tool data disclosure.
//! - Essential omitted evidence produces `unavailable / unsupported-context`.
//! - Project configuration cannot widen a trusted user minimal profile.
//! - Local load observations remain separate and unaffected by the active disclosure profile.

use crate::privacy::ContextProfile;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Summary of field inclusion under a disclosure profile.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProfileDisclosedFields {
    pub current_request: bool,
    pub task_anchor: bool,
    pub explicit_constraints: bool,
    pub candidate_material: bool,
    pub recent_messages: bool,
    pub tool_bodies: bool,
    pub dirty_paths: bool,
}

impl ContextProfile {
    /// Returns the field disclosure policy for this profile.
    pub const fn disclosed_fields(self, no_tools: bool) -> ProfileDisclosedFields {
        match self {
            Self::Standard => ProfileDisclosedFields {
                current_request: true,
                task_anchor: true,
                explicit_constraints: true,
                candidate_material: true,
                recent_messages: true,
                tool_bodies: !no_tools,
                dirty_paths: true,
            },
            Self::Minimal => ProfileDisclosedFields {
                current_request: true,
                task_anchor: true,
                explicit_constraints: true,
                candidate_material: true,
                recent_messages: false,
                tool_bodies: false,
                dirty_paths: false,
            },
        }
    }
}

/// Errors occurring during disclosure profile trust resolution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProfileTrustError {
    /// A project layer attempted to widen a profile established by a higher trust layer.
    ProjectCannotWidenProfile {
        user: ContextProfile,
        project: ContextProfile,
    },
    /// An unknown or invalid profile string was provided.
    InvalidProfile(String),
}

impl fmt::Display for ProfileTrustError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProjectCannotWidenProfile { user, project } => {
                write!(
                    f,
                    "project configuration cannot widen trusted user profile '{user}' to '{project}'"
                )
            }
            Self::InvalidProfile(s) => {
                write!(
                    f,
                    "invalid context profile '{s}'; expected 'standard' or 'minimal'"
                )
            }
        }
    }
}

impl std::error::Error for ProfileTrustError {}

/// Validates that a project-level context profile does not widen a user-level profile.
///
/// Under the trust-layer hierarchy:
/// - User setting is authoritative for baseline trust.
/// - Project configuration is **restrict-only** (`Minimal < Standard`).
/// - If `user == Minimal` and `project == Standard`, returns `Err(ProfileTrustError::ProjectCannotWidenProfile)`.
/// - If `user == Standard` and `project == Minimal`, project successfully narrows to `Minimal`.
pub fn validate_project_profile(
    user: ContextProfile,
    project: ContextProfile,
) -> Result<ContextProfile, ProfileTrustError> {
    if project > user {
        return Err(ProfileTrustError::ProjectCannotWidenProfile { user, project });
    }
    Ok(project)
}

/// Resolves the effective context profile across configuration and CLI trust layers.
///
/// Hierarchy and precedence rules:
/// 1. Built-in default is `ContextProfile::Standard`.
/// 2. Trusted user config (`user`) may set `Standard` or `Minimal`.
/// 3. Project config (`project`) is **restrict-only**:
///    - If user is `Minimal`, project setting `Standard` is rejected with `ProjectCannotWidenProfile`.
///    - If user is `Standard` and project is `Minimal`, the project narrows the profile to `Minimal`.
/// 4. Explicit CLI flag (`cli`) is authoritative: explicit user invocation choice prevails.
pub fn resolve_context_profile(
    user: Option<ContextProfile>,
    project: Option<ContextProfile>,
    cli: Option<ContextProfile>,
) -> Result<ContextProfile, ProfileTrustError> {
    // 1. If CLI specifies a profile, it overrides configuration layers
    if let Some(c) = cli {
        return Ok(c);
    }

    // 2. Determine base profile from trusted user config, defaulting to Standard
    let user_profile = user.unwrap_or(ContextProfile::Standard);

    // 3. Project layer is restrict-only: can keep or narrow, never widen
    if let Some(proj_profile) = project {
        return validate_project_profile(user_profile, proj_profile);
    }

    Ok(user_profile)
}

/// Common phrases indicating that a user prompt depends on previous tool execution or output.
const ESSENTIAL_TOOL_PHRASES: &[&str] = &[
    "tool output",
    "tool result",
    "tool error",
    "command output",
    "command error",
    "command failed",
    "why did it fail",
    "why did the command fail",
    "why did the test fail",
    "fix the test failure",
    "fix the error above",
    "fix the failure",
    "see the error above",
    "output from the tool",
    "result of the tool",
    "the error above",
    "the output above",
];

/// Checks if a request text substantively refers to prior tool execution or results.
pub fn is_essential_tool_reference(text: &str) -> bool {
    let lower = text.to_lowercase();
    ESSENTIAL_TOOL_PHRASES
        .iter()
        .any(|phrase| lower.contains(phrase))
}

/// Calculates the disclosed payload size in bytes for comparison and auditing.
pub fn count_disclosed_bytes(payload: &crate::context::render::RenderedContextPayload) -> usize {
    payload.disclosed_bytes()
}
