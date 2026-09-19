//! Local explicit skill requirement resolution and prompt directive parsing.
//!
//! Enforces contract boundary `p2_explicit_resolution`:
//! - Directives are resolved locally before prompt truncation or secret redaction.
//! - Directives in quoted text, code blocks, and tool output are ignored.
//! - Conflicting positive and negative directives produce `ConflictingDirective` failure.
//! - Hard limit of 32 explicit references; 33 or more references reject immediately.
//! - All-or-nothing resolution: any missing, ambiguous, shadowed, unverified, or
//!   forbidden explicit reference fails with `ExplicitResolutionResult::Unavailable` (Exit 5).
//! - Manual-only skills resolve to `manual_only` references without authorizing file-read bypass.
//! - Low-gate and overflow exact references resolve locally without calling Jev.

use super::resolution::{ExactResolution, ResolvedRoster};
use super::{InvocationKind, InvocationName};
use crate::identity::SkillId;
use crate::limits::{HOOK_STDIN_BYTES, MAX_EXPLICIT_REQUESTS};
use std::collections::{BTreeSet, HashSet};

/// Kind of directive parsed from a user prompt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirectiveKind {
    Require,
    Exclude,
}

/// A parsed skill directive extracted from unquoted user prompt text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedDirective {
    pub target: String,
    pub kind: DirectiveKind,
}

/// Reason why an explicit skill reference failed resolution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnresolvedReason {
    Missing,
    Ambiguous,
    Shadowed,
    Unverified,
    Forbidden,
    ConflictingDirective,
    InvalidName,
}

impl UnresolvedReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Ambiguous => "ambiguous",
            Self::Shadowed => "shadowed",
            Self::Unverified => "unverified",
            Self::Forbidden => "forbidden",
            Self::ConflictingDirective => "conflicting-directive",
            Self::InvalidName => "invalid-name",
        }
    }
}

/// Detailed diagnostics for an unresolved explicit skill requirement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnresolvedRecord {
    pub target: String,
    pub reason: UnresolvedReason,
    pub diagnostic: String,
}

/// A successfully resolved explicit skill reference.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedExplicitSkill {
    pub id: SkillId,
    pub invocation: InvocationName,
    pub kind: InvocationKind,
    pub manual_only: bool,
}

/// Request for resolving explicit requirements before truncation.
#[derive(Clone, Debug, Default)]
pub struct ExplicitResolutionRequest {
    /// Explicit IDs or invocation names from CLI `--require-skill`.
    pub cli_required_skills: Vec<String>,
    /// Explicit IDs or invocation names from CLI `--exclude-skill`.
    pub cli_excluded_skills: Vec<String>,
    /// Explicit skill IDs from normalized context envelope.
    pub context_skill_references: Vec<SkillId>,
    /// Explicit exclusions from normalized context envelope.
    pub context_excluded_skills: Vec<SkillId>,
    /// Raw unredacted user prompt before truncation.
    pub user_prompt: Option<String>,
}

/// Outcome of local explicit resolution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExplicitResolutionResult {
    /// All explicit references resolved successfully. Bypasses Jev and retrieval.
    Resolved {
        skills: Vec<ResolvedExplicitSkill>,
        excluded_skills: Vec<SkillId>,
    },
    /// No explicit requirements specified; proceed to normal ranking and Quill retrieval.
    NoneSpecified { excluded_skills: Vec<SkillId> },
    /// One or more explicit requirements failed resolution.
    /// Maps to Decision::Unavailable / ErrorKind::UnresolvedExplicit (Exit 5).
    Unavailable { unresolved: Vec<UnresolvedRecord> },
}

/// Fatal validation error during explicit resolution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExplicitResolutionError {
    TooManyExplicitReferences { count: usize, limit: usize },
    OversizedInput { bytes: usize, limit: usize },
}

impl std::fmt::Display for ExplicitResolutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooManyExplicitReferences { count, limit } => {
                write!(
                    f,
                    "explicit skill references count {count} exceeds limit {limit}"
                )
            }
            Self::OversizedInput { bytes, limit } => {
                write!(f, "user prompt bytes {bytes} exceeds limit {limit}")
            }
        }
    }
}

impl std::error::Error for ExplicitResolutionError {}

/// Parse explicit directives from prompt text, ignoring quoted strings and code blocks.
pub fn parse_prompt_directives(prompt: &str) -> Vec<ParsedDirective> {
    let mut directives = Vec::new();
    let sanitized = strip_quotes_and_code_blocks(prompt);

    for segment in directive_segments(&sanitized) {
        let trimmed = segment.trim();
        if trimmed.is_empty() {
            continue;
        }

        // 1. Slash command directives: /use-skill <name>, /require-skill <name>, /skill <name>
        if let Some(rest) = trimmed.strip_prefix('/') {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if parts.len() >= 2 {
                let cmd = parts[0].to_ascii_lowercase();
                let target = clean_target_name(parts[1]);
                if !target.is_empty() {
                    if cmd == "use-skill" || cmd == "require-skill" || cmd == "skill" {
                        directives.push(ParsedDirective {
                            target,
                            kind: DirectiveKind::Require,
                        });
                    } else if cmd == "exclude-skill" || cmd == "no-skill" {
                        directives.push(ParsedDirective {
                            target,
                            kind: DirectiveKind::Exclude,
                        });
                    }
                }
            }
            continue;
        }

        // 2. Natural language directives
        let lower = trimmed.to_ascii_lowercase();

        // Check exclusions first ("do not use skill X", "don't use skill X", "exclude skill X")
        if let Some(target) = extract_natural_directive(
            trimmed,
            &lower,
            &[
                "do not use skill ",
                "do not use ",
                "don't use skill ",
                "don't use ",
                "exclude skill ",
            ],
        ) {
            directives.push(ParsedDirective {
                target,
                kind: DirectiveKind::Exclude,
            });
            continue;
        }

        // Check positive directives ("use skill X", "require skill X", "run skill X", "invoke skill X")
        if let Some(target) = extract_natural_directive(
            trimmed,
            &lower,
            &[
                "use skill ",
                "require skill ",
                "run skill ",
                "invoke skill ",
                "execute skill ",
            ],
        ) {
            directives.push(ParsedDirective {
                target,
                kind: DirectiveKind::Require,
            });
        }
    }

    directives
}

/// Internal dots belong to exact invocation names. A terminal dot, or one
/// followed by whitespace, still separates ordinary prose sentences.
fn directive_segments(text: &str) -> impl Iterator<Item = &str> {
    text.match_indices(['\n', ';', '.'])
        .filter(|(offset, delimiter)| {
            *delimiter != "."
                || text[offset + 1..]
                    .chars()
                    .next()
                    .is_none_or(char::is_whitespace)
        })
        .chain(std::iter::once((text.len(), "")))
        .scan(0, |start, (end, delimiter)| {
            let segment = &text[*start..end];
            *start = end + delimiter.len();
            Some(segment)
        })
}

fn extract_natural_directive(original: &str, line: &str, prefixes: &[&str]) -> Option<String> {
    for prefix in prefixes {
        if let Some(pos) = line.find(prefix) {
            // Must be at line start or preceded by punctuation/space
            if pos == 0
                || line.as_bytes()[pos - 1].is_ascii_whitespace()
                || line.as_bytes()[pos - 1] == b'.'
                || line.as_bytes()[pos - 1] == b';'
                || line.as_bytes()[pos - 1] == b','
            {
                // ASCII case folding preserves byte offsets, but only the
                // directive vocabulary is case-insensitive. Targets are exact.
                let after = &original[pos + prefix.len()..];
                let first_word = after.split_whitespace().next()?;
                let target = clean_target_name(first_word);
                if !target.is_empty() {
                    return Some(target);
                }
            }
        }
    }
    None
}

fn clean_target_name(word: &str) -> String {
    word.trim_matches(|c: char| {
        !c.is_alphanumeric() && c != '-' && c != '_' && c != '.' && c != '/'
    })
    .to_string()
}

/// Strip code blocks (` ```...``` ` and inline `` `...` ``) and quoted strings (`"..."`, `'...'`).
/// Preserves English apostrophes / contractions (e.g. "don't", "can't").
fn strip_quotes_and_code_blocks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut i = 0;

    while i < n {
        // 1. Triple backtick code block ```...```
        if i + 2 < n && chars[i] == '`' && chars[i + 1] == '`' && chars[i + 2] == '`' {
            let start = i + 3;
            let mut end = None;
            let mut j = start;
            while j + 2 < n {
                if chars[j] == '`' && chars[j + 1] == '`' && chars[j + 2] == '`' {
                    end = Some(j + 3);
                    break;
                }
                j += 1;
            }
            if let Some(close_idx) = end {
                out.push(' ');
                i = close_idx;
                continue;
            } else {
                out.push(' ');
                break;
            }
        }

        // 2. Single backtick inline code `...`
        if chars[i] == '`' {
            let mut j = i + 1;
            let mut end = None;
            while j < n && chars[j] != '\n' {
                if chars[j] == '`' {
                    end = Some(j + 1);
                    break;
                }
                j += 1;
            }
            if let Some(close_idx) = end {
                out.push(' ');
                i = close_idx;
                continue;
            }
        }

        // 3. Double quotes "..."
        if chars[i] == '"' {
            let mut j = i + 1;
            let mut end = None;
            while j < n {
                if chars[j] == '"' {
                    end = Some(j + 1);
                    break;
                }
                j += 1;
            }
            if let Some(close_idx) = end {
                out.push(' ');
                i = close_idx;
                continue;
            }
        }

        // 4. Single quotes '...'
        if chars[i] == '\'' {
            // Check if this is an apostrophe within a word (e.g. don't, can't, user's)
            let is_apostrophe = (i > 0 && chars[i - 1].is_alphanumeric())
                && (i + 1 < n && chars[i + 1].is_alphanumeric());
            if is_apostrophe {
                out.push('\'');
                i += 1;
                continue;
            }

            // Otherwise, look for matching closing single quote
            let mut j = i + 1;
            let mut end = None;
            while j < n && chars[j] != '\n' {
                if chars[j] == '\'' && !(j + 1 < n && chars[j + 1].is_alphanumeric()) {
                    end = Some(j + 1);
                    break;
                }
                j += 1;
            }
            if let Some(close_idx) = end {
                out.push(' ');
                i = close_idx;
                continue;
            }
        }

        out.push(chars[i]);
        i += 1;
    }

    out
}

/// Resolve all explicit requirements locally against the visible roster.
pub fn resolve_explicit_requirements(
    request: &ExplicitResolutionRequest,
    roster: &ResolvedRoster,
) -> Result<ExplicitResolutionResult, ExplicitResolutionError> {
    // 1. Validate prompt input bounds before parsing
    if let Some(prompt) = &request.user_prompt {
        let bytes = prompt.len();
        if bytes > HOOK_STDIN_BYTES.max() {
            return Err(ExplicitResolutionError::OversizedInput {
                bytes,
                limit: HOOK_STDIN_BYTES.max(),
            });
        }
    }

    // 2. Parse directives from prompt if present
    let prompt_directives = request
        .user_prompt
        .as_deref()
        .map(parse_prompt_directives)
        .unwrap_or_default();

    // 3. Collect and deduplicate positive targets
    let mut positive_targets = Vec::new();
    let mut seen_positive = HashSet::new();

    // Context references
    for id in &request.context_skill_references {
        if seen_positive.insert(id.as_str().to_string()) {
            positive_targets.push(id.as_str().to_string());
        }
    }

    // CLI required skills
    for req in &request.cli_required_skills {
        if seen_positive.insert(req.clone()) {
            positive_targets.push(req.clone());
        }
    }

    // Prompt positive directives
    for d in &prompt_directives {
        if d.kind == DirectiveKind::Require && seen_positive.insert(d.target.clone()) {
            positive_targets.push(d.target.clone());
        }
    }

    // 4. Collect negative targets (exclusions)
    let mut negative_targets = HashSet::new();

    for id in &request.context_excluded_skills {
        negative_targets.insert(id.as_str().to_string());
    }
    for exc in &request.cli_excluded_skills {
        negative_targets.insert(exc.clone());
    }
    for d in &prompt_directives {
        if d.kind == DirectiveKind::Exclude {
            negative_targets.insert(d.target.clone());
        }
    }

    // 5. Enforce 32-reference maximum limit BEFORE resolution
    let max_allowed = MAX_EXPLICIT_REQUESTS.max();
    if positive_targets.len() > max_allowed {
        return Err(ExplicitResolutionError::TooManyExplicitReferences {
            count: positive_targets.len(),
            limit: max_allowed,
        });
    }

    // 6. Check for conflicting directives (target present in both positive and negative)
    let mut unresolved = Vec::new();
    for target in &positive_targets {
        if negative_targets.contains(target) {
            unresolved.push(UnresolvedRecord {
                target: target.clone(),
                reason: UnresolvedReason::ConflictingDirective,
                diagnostic: format!(
                    "conflicting positive and negative directives for skill '{target}'"
                ),
            });
        }
    }

    // If conflicting directives found, immediately fail as Unavailable
    if !unresolved.is_empty() {
        return Ok(ExplicitResolutionResult::Unavailable { unresolved });
    }

    // 7. If no positive explicit requirements were specified, return NoneSpecified
    let resolved_exclusions = resolve_exclusions(&negative_targets, roster);

    if positive_targets.is_empty() {
        return Ok(ExplicitResolutionResult::NoneSpecified {
            excluded_skills: resolved_exclusions,
        });
    }

    // 8. Resolve each positive target against visible roster
    let mut resolved_skills = Vec::new();

    for target in positive_targets {
        // Try resolution by SkillId first if it parses, otherwise by exact name
        let exact_res = if let Ok(skill_id) = SkillId::new(target.clone()) {
            match roster.exact_id(&skill_id) {
                ExactResolution::Missing => roster.exact_name(&target),
                other => other,
            }
        } else {
            roster.exact_name(&target)
        };

        match exact_res {
            ExactResolution::Resolved {
                id,
                invocation,
                kind,
            } => {
                if resolved_exclusions.contains(id) {
                    unresolved.push(UnresolvedRecord {
                        target: target.clone(),
                        reason: UnresolvedReason::ConflictingDirective,
                        diagnostic: format!(
                            "skill '{target}' ({}) conflicts with an explicit exclusion",
                            id.as_str()
                        ),
                    });
                } else {
                    let manual_only = kind == InvocationKind::ManualOnly;
                    resolved_skills.push(ResolvedExplicitSkill {
                        id: id.clone(),
                        invocation: invocation.clone(),
                        kind,
                        manual_only,
                    });
                }
            }
            ExactResolution::Missing => {
                unresolved.push(UnresolvedRecord {
                    target: target.clone(),
                    reason: UnresolvedReason::Missing,
                    diagnostic: format!("skill '{target}' not found in visible roster"),
                });
            }
            ExactResolution::Ambiguous => {
                unresolved.push(UnresolvedRecord {
                    target: target.clone(),
                    reason: UnresolvedReason::Ambiguous,
                    diagnostic: format!("skill '{target}' has ambiguous visibility across sources"),
                });
            }
            ExactResolution::Shadowed => {
                unresolved.push(UnresolvedRecord {
                    target: target.clone(),
                    reason: UnresolvedReason::Shadowed,
                    diagnostic: format!("skill '{target}' is shadowed by a higher-priority root"),
                });
            }
            ExactResolution::Unverified => {
                unresolved.push(UnresolvedRecord {
                    target: target.clone(),
                    reason: UnresolvedReason::Unverified,
                    diagnostic: format!("skill '{target}' has unverified harness visibility"),
                });
            }
            ExactResolution::Forbidden => {
                unresolved.push(UnresolvedRecord {
                    target: target.clone(),
                    reason: UnresolvedReason::Forbidden,
                    diagnostic: format!("skill '{target}' is forbidden by invocation restrictions"),
                });
            }
        }
    }

    // 9. All-or-nothing: if any failed, return Unavailable; else Resolved
    if !unresolved.is_empty() {
        Ok(ExplicitResolutionResult::Unavailable { unresolved })
    } else {
        Ok(ExplicitResolutionResult::Resolved {
            skills: resolved_skills,
            excluded_skills: resolved_exclusions,
        })
    }
}

/// Exclusion is a restriction, not an invocation grant. Match actual identities
/// and invocation names even for ambiguous/unverified bindings, then exclude
/// every binding of that physical skill so another alias cannot bypass it.
/// ID syntax alone cannot distinguish an invocation name from an opaque ID.
fn resolve_exclusions(targets: &HashSet<String>, roster: &ResolvedRoster) -> Vec<SkillId> {
    let mut unmatched: HashSet<&str> = targets.iter().map(String::as_str).collect();
    let mut excluded = BTreeSet::new();
    for skill in roster.skills() {
        let mut matches = false;
        for binding in skill.bindings() {
            for key in [binding.id.as_str(), binding.invocation.as_str()] {
                if targets.contains(key) {
                    unmatched.remove(key);
                    matches = true;
                }
            }
        }
        if matches {
            excluded.extend(skill.bindings().iter().map(|binding| binding.id.clone()));
        }
    }
    // Preserve unknown explicit IDs for downstream policy comparisons, without
    // inventing an additional identity for a successfully matched name.
    excluded.extend(
        unmatched
            .into_iter()
            .filter_map(|target| SkillId::new(target).ok()),
    );
    excluded.into_iter().collect()
}
