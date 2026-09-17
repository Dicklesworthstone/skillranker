//! Pure, bounded field redaction and final JSON inspection.
//!
//! Pattern and overlap ideas adapted from meta_skill's secret_scanner.rs at
//! c9a616bcb29c89e640a95f2bca344c3053fdf7d0; see THIRD_PARTY_NOTICES.md.
//! Patterns cannot identify arbitrary confidential prose. Redact complete fields
//! before truncation; inspect the final serialized request before transmission.

use crate::limits::{
    NORMALIZED_CONTEXT_DEPTH, NORMALIZED_CONTEXT_JSON_BYTES, RENDERED_CONTEXT_SCALARS,
    SERIALIZED_REQUEST_BYTES,
};
use crate::output::JsonSeed;
use regex::Regex;
use serde::de::DeserializeSeed;
use serde_json::Value;
use std::{fmt, ops::Range, sync::LazyLock};

pub const REDACTION_MARKER: &str = "[REDACTED]";
pub const MAX_REDACTED_FIELD_BYTES: usize = NORMALIZED_CONTEXT_JSON_BYTES.max();
pub const MAX_REDACTED_EXCERPT_SCALARS: usize = RENDERED_CONTEXT_SCALARS.max();
pub const MAX_INSPECTED_PAYLOAD_BYTES: usize = SERIALIZED_REQUEST_BYTES.max();
pub const MAX_INSPECTED_PAYLOAD_DEPTH: usize = NORMALIZED_CONTEXT_DEPTH.max();

/// Static diagnostics only; neither matches nor rejected JSON are retained.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RedactionError {
    FieldTooLong,
    OutputTooLong,
    ExcerptTooLong,
    PayloadTooLong,
    InvalidPayload,
    SecretsDetected { count: usize },
}

impl fmt::Display for RedactionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::FieldTooLong => "field exceeds the redaction input bound",
            Self::OutputTooLong => "redacted field exceeds the output bound",
            Self::ExcerptTooLong => "excerpt exceeds the scalar bound",
            Self::PayloadTooLong => "payload exceeds the serialized request bound",
            Self::InvalidPayload => "payload is not valid bounded JSON",
            Self::SecretsDetected { .. } => "payload contains detected secrets",
        })
    }
}

impl std::error::Error for RedactionError {}

// Separators are never consumed for AWS identifiers: adjacent separated keys
// are independent matches. No length ceiling may expose a token suffix.
static AWS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:A[KBSC]IA|ACCA)[A-Z0-9]{16}").expect("static AWS pattern"));
static PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"(?:gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9]{22,}_[A-Za-z0-9_]{59,})",
        r"eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+",
        r"(?i)\bbearer\s+[A-Za-z0-9_\-.~+/]+=*",
        // Capture only the entire value, including quotes if present. Quoted
        // Unicode, escaped quotes, whitespace and unbounded-length values are
        // scanned within the complete-field cap. Unterminated quotes fail closed.
        r#"(?i)\b(?:aws[_-]?secret(?:[_-]?access[_-]?key)?|secret[_-]?key|api[_-]?key|password|passwd|pwd|secret|token|credential)["']?\s*[:=]\s*("(?:\\[\s\S]|[^"\\])*(?:"|\\?$)|'(?:\\[\s\S]|[^'\\])*(?:'|\\?$)|[^\s"',;\}]+)"#,
        // All URI schemes, not only databases. Userinfo as a whole is private.
        r#"(?i)[a-z][a-z0-9+.-]*://([^\s/@"'<>]+)@"#,
        r"xox[baprs]-[0-9]{10,13}-[0-9]{10,13}[A-Za-z0-9-]*",
        r"https://hooks\.slack\.com/services/[A-Za-z0-9_-]+/[A-Za-z0-9_-]+/[A-Za-z0-9_-]+",
    ]
    .into_iter()
    .map(|pattern| Regex::new(pattern).expect("static secret pattern"))
    .collect()
});
static PRIVATE_KEY_HEADER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"-----BEGIN ((?:RSA |DSA |EC |OPENSSH |ENCRYPTED )?PRIVATE KEY|PGP PRIVATE KEY BLOCK)-----")
        .expect("static private key pattern")
});
static ENTROPY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z0-9+/=_\-]{32,}").expect("static entropy pattern"));
static SECRET_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:aws[_-]?secret(?:[_-]?access[_-]?key)?|secret[_-]?key|api[_-]?key|password|passwd|pwd|secret|token|credential)$")
        .expect("static secret key pattern")
});

fn entropy(text: &str) -> f64 {
    let mut counts = [0usize; 128];
    for byte in text.bytes() {
        counts[usize::from(byte)] += 1;
    }
    counts
        .into_iter()
        .filter(|&n| n != 0)
        .map(|n| {
            let p = n as f64 / text.len() as f64;
            -p * p.log2()
        })
        .sum()
}

fn merged_spans(text: &str, scan_entropy: bool) -> Vec<Range<usize>> {
    let mut spans = Vec::new();
    for found in AWS.find_iter(text) {
        let word = |b: u8| b.is_ascii_uppercase() || b.is_ascii_digit();
        let left = found.start() == 0 || !word(text.as_bytes()[found.start() - 1]);
        let right = found.end() == text.len() || !word(text.as_bytes()[found.end()]);
        if left && right {
            spans.push(found.range());
        }
    }
    for pattern in PATTERNS.iter() {
        for capture in pattern.captures_iter(text) {
            if let Some(found) = capture.get(1).or_else(|| capture.get(0)) {
                // The fixed marker is safe on repeated passes, including when
                // attached to an assignment key or URI userinfo.
                if found.as_str().trim_matches(['\'', '"']) != REDACTION_MARKER {
                    spans.push(found.range());
                }
            }
        }
    }
    let mut covered_until = 0;
    for capture in PRIVATE_KEY_HEADER.captures_iter(text) {
        let header = capture.get(0).expect("private key header");
        if header.start() < covered_until {
            continue;
        }
        let label = capture.get(1).expect("private key label").as_str();
        let footer = format!("-----END {label}-----");
        covered_until = text[header.end()..]
            .find(&footer)
            .map_or(text.len(), |offset| header.end() + offset + footer.len());
        spans.push(header.start()..covered_until);
    }
    if scan_entropy {
        for found in ENTROPY.find_iter(text) {
            if entropy(found.as_str()) > 4.5 {
                // Never suppress an overlapping entropy span: it may extend a
                // shorter regex match and protect otherwise exposed suffixes.
                spans.push(found.range());
            }
        }
    }
    spans.sort_unstable_by_key(|span| (span.start, span.end));
    let mut used = 0usize;
    for index in 0..spans.len() {
        if used > 0 && spans[index].start < spans[used - 1].end {
            spans[used - 1].end = spans[used - 1].end.max(spans[index].end);
        } else {
            spans.swap(used, index);
            used += 1;
        }
    }
    spans.truncate(used);
    spans
}

/// Redacted text may still contain private prose: Debug hides all of it.
#[derive(Clone, Eq, PartialEq)]
pub struct RedactedField {
    text: String,
    redactions: usize,
    omitted_scalars: usize,
}

impl fmt::Debug for RedactedField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RedactedField")
            .field("redactions", &self.redactions)
            .field("omitted_scalars", &self.omitted_scalars)
            .finish_non_exhaustive()
    }
}

impl RedactedField {
    pub fn as_str(&self) -> &str {
        &self.text
    }
    pub fn into_string(self) -> String {
        self.text
    }
    pub const fn redaction_count(&self) -> usize {
        self.redactions
    }
    /// Scalars omitted from the redacted text, not original secret lengths.
    pub const fn omitted_scalars(&self) -> usize {
        self.omitted_scalars
    }
}

/// Successful inspection retains counts only, never the payload or matches.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PayloadInspection {
    pub strings_inspected: usize,
}

/// Fixed-bounded scanner with optional source-compatible ASCII entropy pass.
/// Entropy is off by default to avoid treating ordinary hashes as credentials.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Redactor {
    entropy: bool,
}

impl Redactor {
    pub const fn with_entropy(entropy: bool) -> Self {
        Self { entropy }
    }

    /// Scan the entire field, then replace merged spans. Reject oversized
    /// input or expanded output; never silently truncate an unscanned suffix.
    pub fn redact_field(&self, field: &str) -> Result<RedactedField, RedactionError> {
        if field.len() > MAX_REDACTED_FIELD_BYTES {
            return Err(RedactionError::FieldTooLong);
        }
        let spans = merged_spans(field, self.entropy);
        let removed: usize = spans.iter().map(|span| span.len()).sum();
        let output_len = field.len() - removed + spans.len() * REDACTION_MARKER.len();
        if output_len > MAX_REDACTED_FIELD_BYTES {
            return Err(RedactionError::OutputTooLong);
        }
        let mut text = String::with_capacity(output_len);
        let mut cursor = 0;
        for span in &spans {
            text.push_str(&field[cursor..span.start]);
            text.push_str(REDACTION_MARKER);
            cursor = span.end;
        }
        text.push_str(&field[cursor..]);
        Ok(RedactedField {
            text,
            redactions: spans.len(),
            omitted_scalars: 0,
        })
    }

    /// Redact first, then retain ceil(limit/2) head and floor(limit/2) tail
    /// Unicode scalars. The omission count is separate metadata; no marker
    /// consumes an unbudgeted scalar. Zero keeps no text but still scans all.
    pub fn redact_field_excerpt(
        &self,
        field: &str,
        max_scalars: usize,
    ) -> Result<RedactedField, RedactionError> {
        if max_scalars > MAX_REDACTED_EXCERPT_SCALARS {
            return Err(RedactionError::ExcerptTooLong);
        }
        let mut result = self.redact_field(field)?;
        let total = result.text.chars().count();
        if total > max_scalars {
            let head = max_scalars.div_ceil(2);
            let tail = max_scalars / 2;
            let head_end = result
                .text
                .char_indices()
                .nth(head)
                .map_or(result.text.len(), |(i, _)| i);
            let tail_start = if tail == 0 {
                result.text.len()
            } else {
                result
                    .text
                    .char_indices()
                    .nth_back(tail - 1)
                    .map_or(0, |(i, _)| i)
            };
            result.text.replace_range(head_end..tail_start, "");
            result.omitted_scalars = total - max_scalars;
        }
        Ok(result)
    }

    /// Validate final serialized JSON and inspect every decoded key and string,
    /// including escaped tokens. Duplicate keys, trailing data and excess depth
    /// are rejected by the existing bounded parser. Detected secrets cause an
    /// error, never JSON mutation; a clean result grants no network authority.
    pub fn inspect_payload(&self, payload: &[u8]) -> Result<PayloadInspection, RedactionError> {
        if payload.len() > MAX_INSPECTED_PAYLOAD_BYTES {
            return Err(RedactionError::PayloadTooLong);
        }
        let mut deserializer = serde_json::Deserializer::from_slice(payload);
        let value = JsonSeed(0)
            .deserialize(&mut deserializer)
            .map_err(|_| RedactionError::InvalidPayload)?;
        deserializer
            .end()
            .map_err(|_| RedactionError::InvalidPayload)?;
        let mut strings = 0;
        let count = self.inspect_value(&value, 0, false, &mut strings)?;
        if count != 0 {
            return Err(RedactionError::SecretsDetected { count });
        }
        Ok(PayloadInspection {
            strings_inspected: strings,
        })
    }

    fn inspect_value(
        &self,
        value: &Value,
        depth: usize,
        sensitive_key: bool,
        strings: &mut usize,
    ) -> Result<usize, RedactionError> {
        if depth > MAX_INSPECTED_PAYLOAD_DEPTH {
            return Err(RedactionError::InvalidPayload);
        }
        let mut count = 0;
        match value {
            Value::String(text) => {
                *strings += 1;
                count = if sensitive_key && !text.is_empty() && text != REDACTION_MARKER {
                    1
                } else {
                    merged_spans(text, self.entropy).len()
                };
            }
            Value::Array(values) => {
                for value in values {
                    count += self.inspect_value(value, depth + 1, sensitive_key, strings)?;
                }
            }
            Value::Object(values) => {
                for (key, value) in values {
                    *strings += 1;
                    count += merged_spans(key, self.entropy).len();
                    count += self.inspect_value(
                        value,
                        depth + 1,
                        sensitive_key || SECRET_KEY.is_match(key),
                        strings,
                    )?;
                }
            }
            Value::Bool(_) | Value::Number(_) => count = usize::from(sensitive_key),
            Value::Null => {}
        }
        Ok(count)
    }
}
