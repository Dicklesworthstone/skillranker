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

/// Body scalars kept beyond the excerpt so a secret that starts inside the
/// excerpt is seen whole by redaction before the excerpt is cut.
pub const REDACTION_LOOKAHEAD_SCALARS: usize = 1024;

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
    /// The body's first 700 + 1024 scalars: redact this, then cut the excerpt.
    pub body_window: PrivateText,
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
    /// `context: fork`: the harness runs the skill in a forked context, so its
    /// content is not retained in the invoking conversation.
    pub forked_context: bool,
    /// The body uses invocation-time substitutions or command injection, so
    /// identical source bytes do not prove identical rendered content.
    pub dynamic_content: bool,
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
    let body_window = PrivateText::new(
        body_excerpt_text
            .chars()
            .take(BODY_EXCERPT_MAX_SCALARS + REDACTION_LOOKAHEAD_SCALARS)
            .collect::<String>(),
    );

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
        body_window,
        agent_invocable,
        user_invocable,
        usage_kind: fields.usage_kind,
        aliases: fields.aliases,
        tags,
        phases,
        forked_context: fields.forked_context,
        dynamic_content: has_dynamic_content(body),
        parse_warnings: warnings,
        has_frontmatter,
        frontmatter_bytes,
    })
}

// --- Frontmatter extraction ---

fn extract_frontmatter(content: &str) -> Result<(Option<&str>, &str), FrontmatterError> {
    // Frontmatter must start with `---` as the first non-empty line (ignoring leading empty lines)
    let trimmed_start = content.trim_start_matches(['\r', '\n']);
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

/// Claude renders `$ARGUMENTS`, `$N`, `${CLAUDE_…}` and `` !`command` `` when a
/// skill is invoked. Matching is deliberately broad: a false positive only
/// withholds reference-reuse suppression, never grants it.
fn has_dynamic_content(body: &str) -> bool {
    body.contains("!`")
        || body.contains("$ARGUMENTS")
        || body.contains("${CLAUDE_")
        || body
            .as_bytes()
            .windows(2)
            .any(|pair| pair[0] == b'$' && pair[1].is_ascii_digit())
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
    forked_context: bool,
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
            forked_context: false,
        }
    }
}

impl Default for FrontmatterFields {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether the YAML uses an anchor or alias: `&name` or `*name` where a node
/// begins, meaning a value, a sequence item or a flow element. Elsewhere the
/// characters are prose ("Build & deploy", "*.rs") or block scalar content
/// (Markdown emphasis or bullets under `description: |`), which YAML never
/// reads as anchors.
fn uses_anchor_or_alias(yaml: &str) -> bool {
    let node_start = |text: &str| {
        let mut chars = text.trim_start().chars();
        matches!(chars.next(), Some('&' | '*')) && chars.next().is_some_and(|c| !c.is_whitespace())
    };
    let value_starts_node = |value: &str| {
        let value = value.trim();
        match value.strip_prefix('[').or_else(|| value.strip_prefix('{')) {
            Some(inner) => inner.split(',').any(|element| {
                let element = element.trim_end_matches([']', '}']);
                node_start(element.split_once(": ").map_or(element, |(_, v)| v))
            }),
            None => node_start(value),
        }
    };
    // Indentation of the line that opened a block scalar (`|` or `>`).
    let mut block_parent: Option<usize> = None;
    for line in yaml.lines() {
        let text = line.trim();
        if text.is_empty() || text.starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if let Some(parent) = block_parent {
            if indent > parent {
                continue;
            }
            block_parent = None;
        }
        let body = match text.strip_prefix('-') {
            Some(item) if item.is_empty() || item.starts_with(' ') => item.trim_start(),
            _ => text,
        };
        let value = body.split_once(':').map_or(body, |(_, value)| value);
        if value_starts_node(value) || node_start(body) {
            return true;
        }
        if value.trim_start().starts_with(['|', '>']) {
            block_parent = Some(indent);
        }
    }
    false
}

fn parse_yaml_frontmatter(yaml: &str) -> Result<FrontmatterFields, FrontmatterError> {
    // Anchors and aliases are refused outright: they allow amplification.
    if uses_anchor_or_alias(yaml) {
        return Err(FrontmatterError::AliasForbidden);
    }

    let mut fields = FrontmatterFields::new();
    let mut seen_keys = HashSet::new();
    // Mappings nested under the current top-level key, innermost last. Their
    // contents carry no metadata, but YAML forbids duplicate keys at every level.
    let mut nested = NestedMappings::default();

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

        // Anything indented, or a zero-indented sequence item, belongs to the
        // previous top-level key's value.
        if line.starts_with([' ', '\t']) || is_sequence_item(trimmed) {
            nested.track(line.len() - line.trim_start().len(), trimmed)?;
            idx += 1;
            continue;
        }
        nested = NestedMappings::default();

        let colon_pos = line.find(':').ok_or(FrontmatterError::InvalidYamlSyntax)?;
        let key = line[..colon_pos].trim().to_ascii_lowercase();
        // Accepted spellings share one field identity. Checking only the raw
        // key would let an alias overwrite an earlier invocation restriction.
        let key = match key.as_str() {
            "disable_model_invocation" => "disable-model-invocation",
            "user_invocable" => "user-invocable",
            "usage_kind" => "usage",
            "alias" => "aliases",
            "tag" => "tags",
            "phase" => "phases",
            other => other,
        };
        let value_after_colon = line[colon_pos + 1..].trim();

        if !seen_keys.insert(key.to_owned()) {
            return Err(FrontmatterError::DuplicateKey);
        }

        match key {
            "name" => {
                let (val, next_idx) = parse_string_field(value_after_colon, &lines, idx + 1)?;
                fields.name = Some(val.trim().to_string());
                idx = next_idx;
            }
            "description" => {
                let (val, next_idx) = parse_string_field(value_after_colon, &lines, idx + 1)?;
                fields.description = val.trim().to_string();
                idx = next_idx;
            }
            "disable-model-invocation" => {
                let (val, next_idx) = parse_string_field(value_after_colon, &lines, idx + 1)?;
                fields.disable_model_invocation = parse_boolean(&val)?;
                idx = next_idx;
            }
            "user-invocable" => {
                let (val, next_idx) = parse_string_field(value_after_colon, &lines, idx + 1)?;
                fields.user_invocable = parse_boolean(&val)?;
                idx = next_idx;
            }
            "usage" => {
                let (val, next_idx) = parse_string_field(value_after_colon, &lines, idx + 1)?;
                let lower = val.trim().to_ascii_lowercase();
                fields.usage_kind = match lower.as_str() {
                    "reference" => UsageKind::Reference,
                    "workflow" => UsageKind::Workflow,
                    _ => UsageKind::Unknown,
                };
                idx = next_idx;
            }
            "context" => {
                let (val, next_idx) = parse_string_field(value_after_colon, &lines, idx + 1)?;
                fields.forked_context = val.trim().eq_ignore_ascii_case("fork");
                idx = next_idx;
            }
            "aliases" => {
                let (list, next_idx) = parse_list_or_flow(value_after_colon, &lines, idx + 1)?;
                fields.aliases = list;
                idx = next_idx;
            }
            "tags" => {
                let (list, next_idx) = parse_list_or_flow(value_after_colon, &lines, idx + 1)?;
                fields.tags = list;
                idx = next_idx;
            }
            "phases" => {
                let (list, next_idx) = parse_list_or_flow(value_after_colon, &lines, idx + 1)?;
                fields.phases = list;
                idx = next_idx;
            }
            _ => {
                // Unknown field: consume a scalar value and validate its syntax.
                // An empty value may open a nested collection, which the
                // indented-line branch above checks for duplicate keys.
                if strip_comment(value_after_colon).is_empty() {
                    idx += 1;
                } else {
                    let (_, next_idx) = parse_scalar_or_block(value_after_colon, &lines, idx + 1)?;
                    idx = next_idx;
                }
            }
        }
    }

    Ok(fields)
}

/// A field whose value must be a string. An empty value continues on the
/// indented lines below it, as YAML allows; a collection there or on the key's
/// line is not a string. Quoted bracketed text remains a legitimate value, so
/// the source type is checked before unquoting, which loses that distinction.
fn parse_string_field(
    immediate: &str,
    lines: &[&str],
    next_line_idx: usize,
) -> Result<(String, usize), FrontmatterError> {
    let immediate = strip_comment(immediate);
    if immediate.is_empty() {
        let following = lines[next_line_idx.min(lines.len())..]
            .iter()
            .map(|line| line.trim())
            .find(|text| !text.is_empty() && !text.starts_with('#'));
        if following.is_some_and(|text| is_sequence_item(text) || mapping_key(text).is_some()) {
            return Err(FrontmatterError::InvalidYamlSyntax);
        }
        let (source, next_line_idx) = fold_continuation("", lines, next_line_idx);
        if source.starts_with(['[', '{']) {
            return Err(FrontmatterError::InvalidYamlSyntax);
        }
        return Ok((scalar(&source)?, next_line_idx));
    }
    if immediate.starts_with(['[', '{']) {
        return Err(FrontmatterError::InvalidYamlSyntax);
    }
    parse_scalar_or_block(immediate, lines, next_line_idx)
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

    // A plain, quoted or flow value may continue on indented lines.
    let (source, next_line_idx) = fold_continuation(trimmed, lines, next_line_idx);
    Ok((scalar(&source)?, next_line_idx))
}

/// A value's source text: its first line plus the indented or blank lines that
/// continue it, folded as YAML folds multi-line flow scalars. Lines join with a
/// space and each blank line becomes a newline. Returns the index after the
/// last line consumed.
fn fold_continuation(first: &str, lines: &[&str], mut next_line_idx: usize) -> (String, usize) {
    let mut folded = first.trim().to_owned();
    let mut breaks = 0;
    while next_line_idx < lines.len() {
        let line = lines[next_line_idx];
        let text = line.trim();
        if text.is_empty() {
            breaks += 1;
        } else if line.starts_with([' ', '\t']) {
            // Blank lines before the first text line are not content.
            if !folded.is_empty() {
                if breaks > 0 {
                    folded.extend(std::iter::repeat_n('\n', breaks));
                } else {
                    folded.push(' ');
                }
            }
            breaks = 0;
            folded.push_str(text);
        } else {
            break;
        }
        next_line_idx += 1;
    }
    (folded, next_line_idx)
}

/// Decodes one flow scalar: a trailing comment is dropped, a quoted scalar is
/// unquoted with its escapes applied, and a plain scalar must not leave a flow
/// collection unclosed.
fn scalar(source: &str) -> Result<String, FrontmatterError> {
    let source = strip_comment(source);
    if let Some((decoded, rest)) = split_quoted(source)? {
        return if rest.trim().is_empty() {
            Ok(decoded)
        } else {
            // Text after the closing quote is not part of any YAML scalar.
            Err(FrontmatterError::InvalidYamlSyntax)
        };
    }
    let count = |c: char| source.chars().filter(|&x| x == c).count();
    if count('[') != count(']') || count('{') != count('}') {
        return Err(FrontmatterError::InvalidYamlSyntax);
    }
    Ok(source.to_owned())
}

/// `value` without its trailing comment: a `#` at the start or after
/// whitespace, outside any quoted scalar. A quote opens a scalar only where a
/// node can begin, so an apostrophe inside plain text ("don't") opens nothing.
fn strip_comment(value: &str) -> &str {
    let mut quote = None;
    let mut prev: Option<char> = None;
    let mut chars = value.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        match quote {
            Some('"') => match c {
                '\\' => {
                    chars.next();
                }
                '"' => quote = None,
                _ => {}
            },
            Some(_) => {
                if c == '\'' {
                    if chars.peek().is_some_and(|&(_, next)| next == '\'') {
                        chars.next();
                    } else {
                        quote = None;
                    }
                }
            }
            None => {
                let at_boundary =
                    prev.is_none_or(|p| p.is_whitespace() || matches!(p, '[' | '{' | ','));
                match c {
                    '#' if prev.is_none_or(char::is_whitespace) => return value[..at].trim_end(),
                    '"' | '\'' if at_boundary => quote = Some(c),
                    _ => {}
                }
            }
        }
        prev = Some(c);
    }
    value.trim_end()
}

/// A leading single- or double-quoted scalar, decoded, and the text after its
/// closing quote. `None` when `value` does not start with a quote. A missing
/// closing quote or an unknown escape is invalid.
fn split_quoted(value: &str) -> Result<Option<(String, &str)>, FrontmatterError> {
    let mut chars = value.chars();
    let quote = match chars.next() {
        Some(quote @ ('"' | '\'')) => quote,
        _ => return Ok(None),
    };
    let mut decoded = String::new();
    loop {
        let c = chars.next().ok_or(FrontmatterError::InvalidYamlSyntax)?;
        if c == quote {
            // In single quotes, a doubled quote is a literal quote.
            if quote == '\'' && chars.as_str().starts_with('\'') {
                chars.next();
                decoded.push('\'');
                continue;
            }
            return Ok(Some((decoded, chars.as_str())));
        }
        if quote == '"' && c == '\\' {
            decoded.push(double_quoted_escape(&mut chars)?);
        } else {
            decoded.push(c);
        }
    }
}

/// The character a YAML double-quoted escape stands for, consuming the escape
/// after its backslash.
fn double_quoted_escape(chars: &mut std::str::Chars<'_>) -> Result<char, FrontmatterError> {
    let hex = |chars: &mut std::str::Chars<'_>, digits: usize| {
        let text: String = chars.by_ref().take(digits).collect();
        if text.len() != digits || !text.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(FrontmatterError::InvalidYamlSyntax);
        }
        u32::from_str_radix(&text, 16)
            .ok()
            .and_then(char::from_u32)
            .ok_or(FrontmatterError::InvalidYamlSyntax)
    };
    Ok(
        match chars.next().ok_or(FrontmatterError::InvalidYamlSyntax)? {
            '0' => '\0',
            'a' => '\u{7}',
            'b' => '\u{8}',
            't' | '\t' => '\t',
            'n' => '\n',
            'v' => '\u{b}',
            'f' => '\u{c}',
            'r' => '\r',
            'e' => '\u{1b}',
            ' ' => ' ',
            '"' => '"',
            '/' => '/',
            '\\' => '\\',
            'N' => '\u{85}',
            '_' => '\u{a0}',
            'L' => '\u{2028}',
            'P' => '\u{2029}',
            'x' => hex(chars, 2)?,
            'u' => hex(chars, 4)?,
            'U' => hex(chars, 8)?,
            _ => return Err(FrontmatterError::InvalidYamlSyntax),
        },
    )
}

fn is_sequence_item(text: &str) -> bool {
    text == "-" || text.starts_with("- ") || text.starts_with("-\t")
}

/// The key of a block mapping entry, meaning a key followed by `:` and then
/// whitespace or the end of the line, decoded if it is quoted.
fn mapping_key(text: &str) -> Option<String> {
    if text.starts_with(['#', '[', '{', '|', '>', '&', '*', '!']) {
        return None;
    }
    let is_separator = |rest: &str| {
        rest.strip_prefix(':')
            .is_some_and(|after| after.is_empty() || after.starts_with([' ', '\t']))
    };
    if let Some((key, rest)) = split_quoted(text).ok().flatten() {
        return is_separator(rest.trim_start()).then_some(key);
    }
    let text = strip_comment(text);
    text.char_indices()
        .find(|&(at, c)| c == ':' && is_separator(&text[at..]))
        .map(|(at, _)| text[..at].trim_end().to_owned())
}

/// Duplicate-key tracking for the block mappings beneath one top-level key.
#[derive(Default)]
struct NestedMappings {
    /// Open mappings, innermost last: the column of their keys and the keys
    /// seen at that column.
    scopes: Vec<(usize, HashSet<String>)>,
    /// Column of a key whose block scalar content is being skipped.
    block_parent: Option<usize>,
}

impl NestedMappings {
    fn track(&mut self, indent: usize, text: &str) -> Result<(), FrontmatterError> {
        if let Some(parent) = self.block_parent {
            if indent > parent {
                return Ok(());
            }
            self.block_parent = None;
        }
        let (column, entry) = match text.strip_prefix('-') {
            Some(item) if is_sequence_item(text) => {
                // Each sequence item begins a new node; mappings deeper than
                // the item belonged to the previous one.
                self.scopes.retain(|&(column, _)| column <= indent);
                let entry = item.trim_start();
                (indent + (text.len() - entry.len()), entry)
            }
            _ => (indent, text),
        };
        let Some(key) = mapping_key(entry) else {
            return Ok(());
        };
        self.scopes.retain(|&(scope, _)| scope <= column);
        match self.scopes.last_mut() {
            Some((scope, keys)) if *scope == column => {
                if !keys.insert(key) {
                    return Err(FrontmatterError::DuplicateKey);
                }
            }
            _ => {
                if self.scopes.len() >= MAX_FRONTMATTER_DEPTH {
                    return Err(FrontmatterError::NestingTooDeep);
                }
                self.scopes.push((column, HashSet::from([key])));
            }
        }
        let value = entry
            .split_once(':')
            .map_or("", |(_, value)| value)
            .trim_start();
        if value.starts_with(['|', '>']) {
            self.block_parent = Some(column);
        }
        Ok(())
    }
}

fn parse_list_or_flow(
    immediate: &str,
    lines: &[&str],
    mut next_line_idx: usize,
) -> Result<(Vec<String>, usize), FrontmatterError> {
    let trimmed = strip_comment(immediate);

    // Flow sequence: [a, b, c], possibly continued on indented lines
    if trimmed.starts_with('[') {
        let (source, next_line_idx) = fold_continuation(trimmed, lines, next_line_idx);
        let source = strip_comment(&source);
        let inner = source
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
            .ok_or(FrontmatterError::InvalidYamlSyntax)?;
        let mut items = Vec::new();
        for item in split_flow_items(inner) {
            let clean = clean_scalar(item)?;
            if !clean.is_empty() {
                items.push(clean);
            }
        }
        return Ok((items, next_line_idx));
    }

    // A single scalar stands for a one-item list rather than being dropped.
    if !trimmed.is_empty() {
        let (source, next_line_idx) = fold_continuation(trimmed, lines, next_line_idx);
        let clean = clean_scalar(&source)?;
        return Ok((
            Some(clean)
                .filter(|item| !item.is_empty())
                .into_iter()
                .collect(),
            next_line_idx,
        ));
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

/// A list item's value, trimmed.
fn clean_scalar(s: &str) -> Result<String, FrontmatterError> {
    Ok(scalar(s.trim())?.trim().to_string())
}

/// The elements of a flow sequence's interior, split at commas outside quotes.
fn split_flow_items(inner: &str) -> Vec<&str> {
    let mut items = Vec::new();
    let mut start = 0;
    let mut quote = None;
    let mut chars = inner.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        match (quote, c) {
            (Some('"'), '\\') => {
                chars.next();
            }
            (Some('\''), '\'') if chars.peek().is_some_and(|&(_, next)| next == '\'') => {
                chars.next();
            }
            (Some(open), _) if c == open => quote = None,
            (None, '"' | '\'') if inner[start..at].trim().is_empty() => quote = Some(c),
            (None, ',') => {
                items.push(inner[start..at].trim());
                start = at + 1;
            }
            _ => {}
        }
    }
    items.push(inner[start..].trim());
    items
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
