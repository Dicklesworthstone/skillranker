//! Bounded frontmatter and skill metadata parsing.
//!
//! # Embedded Invariants
//! - Limits: 256 KiB max file, 16 KiB max frontmatter.
//! - UTF-8 BOM (`\u{feff}`) and CRLF (`\r\n`) handled transparently.
//! - YAML parsing is pure and bounded; anchors/aliases (`&anchor`, `*alias`) are forbidden.
//! - Duplicate keys within frontmatter are strictly rejected (no last-key-wins).
//! - Missing frontmatter is allowed; falls back to H1 title and first paragraph.
//! - Malformed frontmatter fails with sanitized diagnostics (raw YAML is never echoed).
//! - Headings inside fenced code blocks (` ``` ` or `~~~`) are ignored.
//! - Dynamic command substitutions (`!cmd`, `` `cmd` ``, `$()`, `{{}}`) remain inert text.
//! - Full description is preserved; short wide (160 scalars) and rerank (1000 scalars)
//!   excerpts are deterministically bounded and wrapped in `PrivateText`.

use crate::context::PrivateText;
use crate::output::ErrorKind;
use crate::roster::{ParseWarning, UsageKind};
use std::collections::HashSet;
use std::fmt;

/// Maximum allowed bytes for a skill file (256 KiB).
pub const MAX_SKILL_FILE_BYTES: usize = 256 * 1024;

/// Maximum allowed bytes for YAML frontmatter (16 KiB).
pub const MAX_FRONTMATTER_BYTES: usize = 16 * 1024;

/// Maximum characters (Unicode scalar values) for wide stage description excerpt.
pub const WIDE_DESCRIPTION_MAX_SCALARS: usize = 160;

/// Maximum characters (Unicode scalar values) for rerank stage description.
pub const RERANK_DESCRIPTION_MAX_SCALARS: usize = 1000;

/// Maximum characters (Unicode scalar values) for body excerpt.
pub const BODY_EXCERPT_MAX_SCALARS: usize = 700;

/// Maximum allowed frontmatter nesting depth.
pub const MAX_FRONTMATTER_DEPTH: usize = 8;

/// Parsed metadata extracted from a skill file (`SKILL.md`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedSkillMetadata {
    /// Skill invocation or display name (from frontmatter `name` or H1 fallback).
    pub name: Option<String>,
    /// Full description text.
    pub description: String,
    /// Bounded full description wrapped in `PrivateText`.
    pub description_full: PrivateText,
    /// Truncated wide description excerpt (max 160 characters).
    pub description_short: PrivateText,
    /// Truncated body excerpt (max 700 characters).
    pub body_excerpt: PrivateText,
    /// Whether the model may automatically invoke this skill (`true` unless disabled).
    pub agent_invocable: bool,
    /// Whether the user can manually invoke this skill (default `true`).
    pub user_invocable: bool,
    /// Usage kind (Reference, Workflow, Unknown).
    pub usage_kind: UsageKind,
    /// Callable or legacy aliases.
    pub aliases: Vec<String>,
    /// Associated topic tags.
    pub tags: Vec<PrivateText>,
    /// Declared workflow phases.
    pub phases: Vec<PrivateText>,
    /// Informational warnings during parse (e.g. missing frontmatter).
    pub parse_warnings: Vec<ParseWarning>,
    /// Whether YAML frontmatter was present.
    pub has_frontmatter: bool,
    /// Byte length of the raw frontmatter.
    pub frontmatter_bytes: usize,
}

/// Errors encountered while reading or parsing skill metadata.
///
/// Contains NO raw YAML or untrusted input text to prevent diagnostic leaks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrontmatterError {
    /// File size exceeds 256 KiB limit.
    FileTooLarge(usize),
    /// Frontmatter byte length exceeds 16 KiB limit.
    FrontmatterTooLarge(usize),
    /// Frontmatter delimiter `---` was not closed.
    UnclosedFrontmatter,
    /// Duplicate key in YAML frontmatter.
    DuplicateKey,
    /// YAML anchors/aliases (`&anchor`, `*alias`) are forbidden.
    AliasForbidden,
    /// Invalid YAML structure or syntax.
    InvalidYamlSyntax,
    /// Nesting depth exceeded maximum bound.
    NestingTooDeep,
    /// Invalid UTF-8 sequence.
    InvalidUtf8,
}

impl FrontmatterError {
    pub const fn kind(&self) -> ErrorKind {
        match self {
            Self::FileTooLarge(_) | Self::FrontmatterTooLarge(_) => ErrorKind::OversizedInput,
            _ => ErrorKind::MalformedInput,
        }
    }
}

impl fmt::Display for FrontmatterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FileTooLarge(len) => {
                write!(
                    f,
                    "skill file exceeds size limit ({len} bytes > {MAX_SKILL_FILE_BYTES})"
                )
            }
            Self::FrontmatterTooLarge(len) => {
                write!(
                    f,
                    "frontmatter exceeds size limit ({len} bytes > {MAX_FRONTMATTER_BYTES})"
                )
            }
            Self::UnclosedFrontmatter => {
                f.write_str("unclosed frontmatter: missing closing '---' or '...' delimiter")
            }
            Self::DuplicateKey => f.write_str("duplicate key rejected in skill frontmatter"),
            Self::AliasForbidden => {
                f.write_str("YAML anchors and aliases are forbidden in skill frontmatter")
            }
            Self::InvalidYamlSyntax => f.write_str("malformed YAML syntax in skill frontmatter"),
            Self::NestingTooDeep => {
                write!(
                    f,
                    "frontmatter nesting exceeds maximum allowed depth ({MAX_FRONTMATTER_DEPTH})"
                )
            }
            Self::InvalidUtf8 => f.write_str("skill content contains invalid UTF-8 bytes"),
        }
    }
}

impl std::error::Error for FrontmatterError {}

/// Parse a skill markdown document into bounded `ParsedSkillMetadata`.
pub fn parse_skill_metadata(content_bytes: &[u8]) -> Result<ParsedSkillMetadata, FrontmatterError> {
    if content_bytes.len() > MAX_SKILL_FILE_BYTES {
        return Err(FrontmatterError::FileTooLarge(content_bytes.len()));
    }

    let content_str =
        std::str::from_utf8(content_bytes).map_err(|_| FrontmatterError::InvalidUtf8)?;

    // Strip UTF-8 BOM if present
    let content = content_str.strip_prefix('\u{feff}').unwrap_or(content_str);

    // Check for frontmatter
    let (frontmatter_opt, body) = extract_frontmatter(content)?;

    let mut warnings = Vec::new();
    let (fields, has_frontmatter, frontmatter_bytes) = match frontmatter_opt {
        Some(fm_str) => {
            let bytes = fm_str.len();
            let parsed_fields = parse_yaml_frontmatter(fm_str)?;
            (parsed_fields, true, bytes)
        }
        None => {
            warnings.push(ParseWarning::MissingFrontmatter);
            (FrontmatterFields::default(), false, 0)
        }
    };

    // Extract markdown fallback info (H1 title and first paragraph)
    let (h1_title, first_paragraph, body_excerpt_text) = parse_markdown_elements(body);

    // Resolve name: frontmatter `name` takes precedence, then H1 title
    let name = fields.name.or(h1_title);

    // Resolve description: frontmatter `description` takes precedence, then first paragraph
    let description = if !fields.description.is_empty() {
        fields.description
    } else {
        first_paragraph
    };

    // Bounded descriptions
    let description_full = PrivateText::new(&description);

    let short_chars: String = description
        .chars()
        .take(WIDE_DESCRIPTION_MAX_SCALARS)
        .collect();
    let description_short = PrivateText::new(short_chars);

    let body_chars: String = body_excerpt_text
        .chars()
        .take(BODY_EXCERPT_MAX_SCALARS)
        .collect();
    let body_excerpt = PrivateText::new(body_chars);

    let agent_invocable = !fields.disable_model_invocation;
    let user_invocable = fields.user_invocable;

    let tags = fields.tags.into_iter().map(PrivateText::new).collect();
    let phases = fields.phases.into_iter().map(PrivateText::new).collect();

    Ok(ParsedSkillMetadata {
        name,
        description,
        description_full,
        description_short,
        body_excerpt,
        agent_invocable,
        user_invocable,
        usage_kind: fields.usage_kind,
        aliases: fields.aliases,
        tags,
        phases,
        parse_warnings: warnings,
        has_frontmatter,
        frontmatter_bytes,
    })
}

// --- Frontmatter extraction ---

fn extract_frontmatter(content: &str) -> Result<(Option<&str>, &str), FrontmatterError> {
    // Frontmatter must start with `---` as the first non-empty line (ignoring leading empty lines)
    let trimmed_start = content.trim_start_matches(|c: char| c == '\r' || c == '\n');
    if !trimmed_start.starts_with("---") {
        return Ok((None, content));
    }

    // Ensure the delimiter line is strictly `---` (optionally followed by \r\n or whitespace)
    let after_first = &trimmed_start[3..];
    let line_end = after_first.find('\n').unwrap_or(after_first.len());
    let rest_of_line = after_first[..line_end].trim();
    if !rest_of_line.is_empty() {
        // Line was something like `---something`, which is not a frontmatter start
        return Ok((None, content));
    }

    let fm_start = if line_end < after_first.len() {
        &after_first[line_end + 1..]
    } else {
        ""
    };

    // Find closing delimiter `---` or `...` on its own line
    let mut offset = 0;
    let mut found_end = None;

    for line in fm_start.lines() {
        let line_len = line.len();
        let trimmed = line.trim();
        if trimmed == "---" || trimmed == "..." {
            found_end = Some(offset);
            break;
        }
        // Advance offset by line length plus newline character(s)
        // fm_start[offset..] starts with line
        offset += line_len;
        if fm_start[offset..].starts_with("\r\n") {
            offset += 2;
        } else if fm_start[offset..].starts_with('\n') {
            offset += 1;
        }
    }

    let end_offset = found_end.ok_or(FrontmatterError::UnclosedFrontmatter)?;
    let fm_raw = &fm_start[..end_offset];

    if fm_raw.len() > MAX_FRONTMATTER_BYTES {
        return Err(FrontmatterError::FrontmatterTooLarge(fm_raw.len()));
    }

    // Body is everything after the closing delimiter line
    let after_closing = &fm_start[end_offset..];
    let closing_line_end = after_closing.find('\n').unwrap_or(after_closing.len());
    let body = if closing_line_end < after_closing.len() {
        &after_closing[closing_line_end + 1..]
    } else {
        ""
    };

    Ok((Some(fm_raw), body))
}

// --- Frontmatter YAML Parser ---

struct FrontmatterFields {
    name: Option<String>,
    description: String,
    disable_model_invocation: bool,
    user_invocable: bool,
    usage_kind: UsageKind,
    aliases: Vec<String>,
    tags: Vec<String>,
    phases: Vec<String>,
}

impl FrontmatterFields {
    fn new() -> Self {
        Self {
            name: None,
            description: String::new(),
            disable_model_invocation: false,
            user_invocable: true, // Default true
            usage_kind: UsageKind::Unknown,
            aliases: Vec::new(),
            tags: Vec::new(),
            phases: Vec::new(),
        }
    }
}

impl Default for FrontmatterFields {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_yaml_frontmatter(yaml: &str) -> Result<FrontmatterFields, FrontmatterError> {
    // Check for YAML anchors/aliases to prevent amplification attacks
    if yaml.contains('&') || yaml.contains('*') {
        for line in yaml.lines() {
            let t = line.trim();
            if t.starts_with('&') || t.starts_with('*') || t.contains(" &") || t.contains(" *") {
                return Err(FrontmatterError::AliasForbidden);
            }
        }
    }

    let mut fields = FrontmatterFields::new();
    let mut seen_keys = HashSet::new();

    let lines: Vec<&str> = yaml.lines().collect();
    let mut idx = 0;

    while idx < lines.len() {
        let line = lines[idx];
        let trimmed = line.trim();

        // Skip comments and empty lines
        if trimmed.is_empty() || trimmed.starts_with('#') {
            idx += 1;
            continue;
        }

        // Top-level key: must not be indented
        if line.starts_with(' ') || line.starts_with('\t') {
            idx += 1;
            continue;
        }

        let colon_pos = line.find(':').ok_or(FrontmatterError::InvalidYamlSyntax)?;
        let key = line[..colon_pos].trim().to_ascii_lowercase();
        let value_after_colon = line[colon_pos + 1..].trim();

        if !seen_keys.insert(key.clone()) {
            return Err(FrontmatterError::DuplicateKey);
        }

        match key.as_str() {
            "name" => {
                let (val, next_idx) = parse_scalar_or_block(value_after_colon, &lines, idx + 1)?;
                fields.name = Some(val.trim().to_string());
                idx = next_idx;
            }
            "description" => {
                let (val, next_idx) = parse_scalar_or_block(value_after_colon, &lines, idx + 1)?;
                fields.description = val.trim().to_string();
                idx = next_idx;
            }
            "disable-model-invocation" | "disable_model_invocation" => {
                let (val, next_idx) = parse_scalar_or_block(value_after_colon, &lines, idx + 1)?;
                fields.disable_model_invocation = parse_boolean(&val)?;
                idx = next_idx;
            }
            "user-invocable" | "user_invocable" => {
                let (val, next_idx) = parse_scalar_or_block(value_after_colon, &lines, idx + 1)?;
                fields.user_invocable = parse_boolean(&val)?;
                idx = next_idx;
            }
            "usage" | "usage_kind" => {
                let (val, next_idx) = parse_scalar_or_block(value_after_colon, &lines, idx + 1)?;
                let lower = val.trim().to_ascii_lowercase();
                fields.usage_kind = match lower.as_str() {
                    "reference" => UsageKind::Reference,
                    "workflow" => UsageKind::Workflow,
                    _ => UsageKind::Unknown,
                };
                idx = next_idx;
            }
            "aliases" | "alias" => {
                let (list, next_idx) = parse_list_or_flow(value_after_colon, &lines, idx + 1)?;
                fields.aliases = list;
                idx = next_idx;
            }
            "tags" | "tag" => {
                let (list, next_idx) = parse_list_or_flow(value_after_colon, &lines, idx + 1)?;
                fields.tags = list;
                idx = next_idx;
            }
            "phases" | "phase" => {
                let (list, next_idx) = parse_list_or_flow(value_after_colon, &lines, idx + 1)?;
                fields.phases = list;
                idx = next_idx;
            }
            _ => {
                // Unknown field: consume safely without failure (bounded forward scan)
                let (_, next_idx) = parse_scalar_or_block(value_after_colon, &lines, idx + 1)?;
                idx = next_idx;
            }
        }
    }

    Ok(fields)
}

fn parse_boolean(val: &str) -> Result<bool, FrontmatterError> {
    let s = val.trim().to_ascii_lowercase();
    match s.as_str() {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" | "" => Ok(false),
        _ => Err(FrontmatterError::InvalidYamlSyntax),
    }
}

fn parse_scalar_or_block(
    immediate: &str,
    lines: &[&str],
    mut next_line_idx: usize,
) -> Result<(String, usize), FrontmatterError> {
    let trimmed = immediate.trim();

    // Check for block scalar indicators (| or >)
    if trimmed.starts_with('|') || trimmed.starts_with('>') {
        let is_folded = trimmed.starts_with('>');
        let mut block_lines = Vec::new();

        while next_line_idx < lines.len() {
            let line = lines[next_line_idx];
            if line.trim().is_empty() {
                block_lines.push("");
                next_line_idx += 1;
                continue;
            }
            // Must be indented
            if line.starts_with(' ') || line.starts_with('\t') {
                block_lines.push(line.trim_start());
                next_line_idx += 1;
            } else {
                break;
            }
        }

        let result = if is_folded {
            // Folded: join adjacent non-empty lines with space, empty lines with newline
            let mut folded = String::new();
            for (i, bl) in block_lines.iter().enumerate() {
                if bl.is_empty() {
                    folded.push('\n');
                } else {
                    if i > 0 && !folded.ends_with('\n') && !folded.is_empty() {
                        folded.push(' ');
                    }
                    folded.push_str(bl);
                }
            }
            folded
        } else {
            // Literal: preserve newlines
            block_lines.join("\n")
        };

        return Ok((result, next_line_idx));
    }

    // Quoted strings
    if (trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2)
        || (trimmed.starts_with('\'') && trimmed.ends_with('\'') && trimmed.len() >= 2)
    {
        let unquoted = &trimmed[1..trimmed.len() - 1];
        return Ok((unquoted.to_string(), next_line_idx));
    }

    // Check for unclosed quotes
    if trimmed.starts_with('"') || trimmed.starts_with('\'') {
        return Err(FrontmatterError::InvalidYamlSyntax);
    }

    // Check for unclosed flow collections in unquoted scalar
    let open_brackets = trimmed.chars().filter(|&c| c == '[').count();
    let close_brackets = trimmed.chars().filter(|&c| c == ']').count();
    if open_brackets != close_brackets {
        return Err(FrontmatterError::InvalidYamlSyntax);
    }

    let open_braces = trimmed.chars().filter(|&c| c == '{').count();
    let close_braces = trimmed.chars().filter(|&c| c == '}').count();
    if open_braces != close_braces {
        return Err(FrontmatterError::InvalidYamlSyntax);
    }

    // Plain scalar
    Ok((trimmed.to_string(), next_line_idx))
}

fn parse_list_or_flow(
    immediate: &str,
    lines: &[&str],
    mut next_line_idx: usize,
) -> Result<(Vec<String>, usize), FrontmatterError> {
    let trimmed = immediate.trim();

    // Flow sequence: [a, b, c]
    if trimmed.starts_with('[') {
        if !trimmed.ends_with(']') {
            return Err(FrontmatterError::InvalidYamlSyntax);
        }
        let inner = &trimmed[1..trimmed.len() - 1];
        let mut items = Vec::new();
        for item in inner.split(',') {
            let clean = clean_scalar(item.trim())?;
            if !clean.is_empty() {
                items.push(clean);
            }
        }
        return Ok((items, next_line_idx));
    }

    // Block sequence: - item
    let mut items = Vec::new();
    while next_line_idx < lines.len() {
        let line = lines[next_line_idx];
        let line_trim = line.trim();
        if line_trim.is_empty() || line_trim.starts_with('#') {
            next_line_idx += 1;
            continue;
        }
        let indent = line.chars().take_while(|&c| c == ' ').count();
        if indent > MAX_FRONTMATTER_DEPTH * 2 {
            return Err(FrontmatterError::NestingTooDeep);
        }
        if line.starts_with(' ') || line.starts_with('\t') || line.starts_with('-') {
            if let Some(item_val) = line_trim.strip_prefix('-') {
                let clean = clean_scalar(item_val.trim())?;
                if !clean.is_empty() {
                    items.push(clean);
                }
                next_line_idx += 1;
            } else {
                break;
            }
        } else {
            break;
        }
    }

    Ok((items, next_line_idx))
}

fn clean_scalar(s: &str) -> Result<String, FrontmatterError> {
    let t = s.trim();
    if (t.starts_with('"') && t.ends_with('"') && t.len() >= 2)
        || (t.starts_with('\'') && t.ends_with('\'') && t.len() >= 2)
    {
        Ok(t[1..t.len() - 1].trim().to_string())
    } else if t.starts_with('"') || t.starts_with('\'') {
        Err(FrontmatterError::InvalidYamlSyntax)
    } else {
        Ok(t.to_string())
    }
}

// --- Markdown Elements Parser (H1, First Paragraph, Body Excerpt) ---

fn parse_markdown_elements(body: &str) -> (Option<String>, String, String) {
    let mut h1_title = None;
    let mut first_paragraph_lines = Vec::new();
    let mut body_lines = Vec::new();

    let mut in_code_fence = false;
    let mut found_h1 = false;
    let mut in_paragraph = false;
    let mut paragraph_done = false;

    for line in body.lines() {
        let trimmed_start = line.trim_start();

        // Code fence toggle
        if trimmed_start.starts_with("```") || trimmed_start.starts_with("~~~") {
            in_code_fence = !in_code_fence;
            body_lines.push(line);
            continue;
        }

        // Headings inside code fences are code, not markdown headings
        if !in_code_fence {
            if let Some(title) = line.strip_prefix("# ") {
                if !found_h1 {
                    h1_title = Some(title.trim().to_string());
                    found_h1 = true;
                    // Start looking for the first paragraph after H1
                    in_paragraph = true;
                    continue;
                }
            } else if line.starts_with("## ") || line.starts_with("### ") {
                // Section header terminates the first paragraph
                in_paragraph = false;
                paragraph_done = true;
            }
        }

        // Collect body lines (excluding top-level H1 title)
        body_lines.push(line);

        // First paragraph extraction
        if !paragraph_done {
            let line_trim = line.trim();
            if in_paragraph {
                if line_trim.is_empty() {
                    if !first_paragraph_lines.is_empty() {
                        paragraph_done = true;
                    }
                } else if !line_trim.starts_with('#') && !in_code_fence {
                    first_paragraph_lines.push(line_trim);
                }
            } else if !line_trim.is_empty() && !line_trim.starts_with('#') && !in_code_fence {
                // Paragraph before H1
                first_paragraph_lines.push(line_trim);
                in_paragraph = true;
            }
        }
    }

    let first_paragraph = first_paragraph_lines.join(" ").trim().to_string();
    let body_excerpt = body_lines.join("\n").trim().to_string();

    (h1_title, first_paragraph, body_excerpt)
}
