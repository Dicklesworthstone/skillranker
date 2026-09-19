//! Safe, readable table rendering and explicit decision views for terminal output.
//!
//! Satisfies contract boundary `p4_table_and_decision_views` (sr-roadmap-l1i.5.13).
//!
//! Enforces:
//! 1. Complete sanitization of ANSI escape sequences and terminal control characters.
//!    Protects against malicious ANSI names, OSC injection, and screen tampering.
//! 2. Accurate visual cell width calculation for Unicode (ASCII = 1, East Asian Wide/Emoji = 2,
//!    combining marks = 0).
//! 3. Column-aligned tables for `Ranked`, `Explicit`, `Abstain`, and `Unavailable` decisions.
//! 4. Separation of stdout (structured data/tables) and stderr (diagnostics).
//! 5. Never auto-entering the TUI, never executing skills or shell commands.

use serde_json::Value;

use super::{Decision, OutputDocument, OutputKind};

/// Calculates the visual terminal display width of a Unicode character.
/// Printable ASCII is 1 column. Fullwidth / CJK / Emoji are 2 columns.
/// Combining characters and zero-width codes are 0 columns.
pub fn char_width(c: char) -> usize {
    let u = c as u32;
    // Control characters have 0 display width (and will be stripped or replaced).
    if u <= 0x1F || (0x7F..=0x9F).contains(&u) {
        return 0;
    }
    // Combining marks (accents, zero-width joiners, etc.)
    if (0x0300..=0x036F).contains(&u)
        || (0x1AB0..=0x1AFF).contains(&u)
        || (0x1DC0..=0x1DFF).contains(&u)
        || (0x20D0..=0x20FF).contains(&u)
        || (0xFE20..=0xFE2F).contains(&u)
        || (0x200B..=0x200F).contains(&u)
        || u == 0xFEFF
    {
        return 0;
    }
    // East Asian Wide / Fullwidth and Emoji
    if (0x1100..=0x115F).contains(&u) // Hangul Jamo
        || (0x2329..=0x232A).contains(&u)
        || (0x2E80..=0x303E).contains(&u) // CJK Radicals, Punctuation
        || (0x3040..=0xA4CF).contains(&u) // Hiragana, Katakana, Bopomofo, Hangul, CJK Ideographs
        || (0xAC00..=0xD7A3).contains(&u) // Hangul Syllables
        || (0xF900..=0xFAFF).contains(&u) // CJK Compatibility Ideographs
        || (0xFE10..=0xFE19).contains(&u) // Vertical forms
        || (0xFE30..=0xFE6F).contains(&u) // CJK Compatibility Forms
        || (0xFF00..=0xFF60).contains(&u) // Fullwidth ASCII variants
        || (0xFFE0..=0xFFE6).contains(&u) // Fullwidth signs
        || (0x1F300..=0x1F9FF).contains(&u) // Misc symbols & Pictographs / Emoji
        || (0x2600..=0x27BF).contains(&u)
    // Misc symbols, Dingbats
    {
        return 2;
    }
    1
}

/// Calculates the visual terminal display width of a string.
pub fn str_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// Strips all ANSI escape sequences and non-printable control characters.
/// Tabs are replaced with a single space. Newlines are preserved.
/// Control codes are replaced with space.
pub fn sanitize_terminal_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Check for CSI sequence: \x1b[ ... [A-Za-z~]
            if chars.peek() == Some(&'[') {
                chars.next(); // consume '['
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if next.is_ascii_alphabetic() || next == '~' {
                        break;
                    }
                }
            } else if chars.peek() == Some(&']') {
                // OSC sequence: \x1b] ... (\x07 | \x1b\)
                chars.next(); // consume ']'
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if next == '\x07' {
                        break;
                    }
                    if next == '\x1b' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            } else {
                // Other escape: consume next character if any
                let _ = chars.next();
            }
        } else if c.is_control() {
            if c == '\t' {
                out.push(' ');
            } else if c == '\n' {
                out.push('\n');
            } else {
                out.push(' ');
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Pads a string with spaces to achieve exact `target_width` in terminal display columns.
pub fn pad_to_width(s: &str, target_width: usize, left_align: bool) -> String {
    let current_width = str_width(s);
    if current_width >= target_width {
        return s.to_string();
    }
    let padding = " ".repeat(target_width - current_width);
    if left_align {
        format!("{s}{padding}")
    } else {
        format!("{padding}{s}")
    }
}

/// Truncates a string to at most `max_width` display columns without breaking multi-byte
/// character boundaries.
pub fn truncate_to_width(s: &str, max_width: usize) -> String {
    let mut width = 0;
    let mut out = String::new();
    for c in s.chars() {
        let cw = char_width(c);
        if width + cw > max_width {
            break;
        }
        out.push(c);
        width += cw;
    }
    out
}

/// Renders an `OutputDocument` into a safe, aligned, readable table view.
pub fn render_table(doc: &OutputDocument) -> String {
    let val = doc.as_value();
    let mut out = match doc.kind() {
        OutputKind::Decision(Decision::Ranked) => render_ranked_table(val),
        OutputKind::Decision(Decision::Explicit) => render_explicit_view(val),
        OutputKind::Decision(Decision::Abstain) => render_abstain_view(val),
        OutputKind::Decision(Decision::Unavailable) => render_unavailable_view(val),
        OutputKind::Artifact(_) => render_artifact_view(val),
    };
    if let Some(trace) = val.get("trace") {
        out.push_str(&render_trace_section(trace));
    }
    out
}

fn render_ranked_table(val: &Value) -> String {
    let mut out = String::new();
    let empty_vec = Vec::new();
    let skills = val["skills"].as_array().unwrap_or(&empty_vec);

    if skills.is_empty() {
        return "No ranked skills found.\n".to_string();
    }

    // Determine column widths
    let mut cmd_width = 7usize; // min "COMMAND"
    let mut id_width = 8usize; // min "SKILL ID"
    let mut name_width = 4usize; // min "NAME"

    for s in skills {
        let raw_cmd = s["invocation_name"].as_str().unwrap_or("");
        let raw_id = s["skill_id"].as_str().unwrap_or("");
        let raw_name = s["name"].as_str().unwrap_or("");
        let clean_cmd = sanitize_terminal_text(raw_cmd);
        let clean_id = sanitize_terminal_text(raw_id);
        let clean_name = sanitize_terminal_text(raw_name);
        cmd_width = cmd_width.max(str_width(&clean_cmd));
        id_width = id_width.max(str_width(&clean_id));
        name_width = name_width.max(str_width(&clean_name));
    }
    cmd_width = cmd_width.min(24);
    id_width = id_width.min(24);
    name_width = name_width.min(32);

    // Headers
    let h_rank = pad_to_width("RANK", 4, false);
    let h_cmd = pad_to_width("COMMAND", cmd_width, true);
    let h_id = pad_to_width("SKILL ID", id_width, true);
    let h_score = pad_to_width("SCORE", 9, false);
    let h_prob = pad_to_width("PROB", 9, false);
    let h_fit = pad_to_width("FIT", 9, false);
    let h_name = pad_to_width("NAME", name_width, true);

    out.push_str(&format!(
        "{h_rank}  {h_cmd}  {h_id}  {h_score}  {h_prob}  {h_fit}  {h_name}\n"
    ));

    // Separators
    let s_rank = "-".repeat(4);
    let s_cmd = "-".repeat(cmd_width);
    let s_id = "-".repeat(id_width);
    let s_score = "-".repeat(9);
    let s_prob = "-".repeat(9);
    let s_fit = "-".repeat(9);
    let s_name = "-".repeat(name_width);
    out.push_str(&format!(
        "{s_rank}  {s_cmd}  {s_id}  {s_score}  {s_prob}  {s_fit}  {s_name}\n"
    ));

    // Data rows
    for s in skills {
        let rank_num = s["rank"].as_u64().unwrap_or(0);
        let score = s["rank_score"].as_f64().unwrap_or(0.0);
        let prob = s["rerank_probability"].as_f64().unwrap_or(0.0);
        let fit = s["fits"].as_f64().unwrap_or(0.0);

        let clean_cmd = sanitize_terminal_text(s["invocation_name"].as_str().unwrap_or(""));
        let clean_id = sanitize_terminal_text(s["skill_id"].as_str().unwrap_or(""));
        let clean_name = sanitize_terminal_text(s["name"].as_str().unwrap_or(""));

        let trunc_cmd = truncate_to_width(&clean_cmd, cmd_width);
        let trunc_id = truncate_to_width(&clean_id, id_width);
        let trunc_name = truncate_to_width(&clean_name, name_width);

        let c_rank = pad_to_width(&rank_num.to_string(), 4, false);
        let c_cmd = pad_to_width(&trunc_cmd, cmd_width, true);
        let c_id = pad_to_width(&trunc_id, id_width, true);
        let c_score = pad_to_width(&format!("{score:.6}"), 9, false);
        let c_prob = pad_to_width(&format!("{prob:.6}"), 9, false);
        let c_fit = pad_to_width(&format!("{fit:.6}"), 9, false);
        let c_name = pad_to_width(&trunc_name, name_width, true);

        out.push_str(&format!(
            "{c_rank}  {c_cmd}  {c_id}  {c_score}  {c_prob}  {c_fit}  {c_name}\n"
        ));
    }

    // Omitted mass or warnings footer
    if let Some(omitted) = val["omitted_rank_mass"]
        .as_f64()
        .or_else(|| val["omitted_mass"].as_f64())
        && omitted > 1e-4
    {
        out.push_str(&format!(
            "\nNote: {:.4} probability mass omitted outside top {}.\n",
            omitted,
            skills.len()
        ));
    }

    out
}

fn render_explicit_view(val: &Value) -> String {
    let mut out = String::new();
    out.push_str("EXPLICIT SKILL DIRECTIVE\n");
    out.push_str("========================\n");
    out.push_str("Local directive resolved. Provider inference was bypassed.\n\n");

    let empty_vec = Vec::new();
    let skills = val["skills"].as_array().unwrap_or(&empty_vec);

    if skills.is_empty() {
        out.push_str("No explicit skills resolved.\n");
        return out;
    }

    let mut cmd_width = 7usize;
    let mut id_width = 8usize;
    let mut name_width = 4usize;
    for s in skills {
        let clean_cmd = sanitize_terminal_text(s["invocation_name"].as_str().unwrap_or(""));
        let clean_id = sanitize_terminal_text(s["skill_id"].as_str().unwrap_or(""));
        let clean_name = sanitize_terminal_text(s["name"].as_str().unwrap_or(""));
        cmd_width = cmd_width.max(str_width(&clean_cmd));
        id_width = id_width.max(str_width(&clean_id));
        name_width = name_width.max(str_width(&clean_name));
    }
    cmd_width = cmd_width.min(24);
    id_width = id_width.min(32);
    name_width = name_width.min(32);

    let h_cmd = pad_to_width("COMMAND", cmd_width, true);
    let h_id = pad_to_width("SKILL ID", id_width, true);
    let h_name = pad_to_width("NAME", name_width, true);
    let h_status = "STATUS";

    out.push_str(&format!("{h_cmd}  {h_id}  {h_name}  {h_status}\n"));
    out.push_str(&format!(
        "{}  {}  {}  ------\n",
        "-".repeat(cmd_width),
        "-".repeat(id_width),
        "-".repeat(name_width)
    ));

    for s in skills {
        let clean_cmd = sanitize_terminal_text(s["invocation_name"].as_str().unwrap_or(""));
        let clean_id = sanitize_terminal_text(s["skill_id"].as_str().unwrap_or(""));
        let clean_name = sanitize_terminal_text(s["name"].as_str().unwrap_or(""));
        let c_cmd = pad_to_width(&truncate_to_width(&clean_cmd, cmd_width), cmd_width, true);
        let c_id = pad_to_width(&truncate_to_width(&clean_id, id_width), id_width, true);
        let c_name = pad_to_width(
            &truncate_to_width(&clean_name, name_width),
            name_width,
            true,
        );
        out.push_str(&format!("{c_cmd}  {c_id}  {c_name}  Required\n"));
    }

    out
}

fn render_abstain_view(val: &Value) -> String {
    let mut out = String::new();
    out.push_str("DECISION: ABSTAIN\n");
    out.push_str("=================\n");
    let reason = sanitize_terminal_text(val["reason"].as_str().unwrap_or("unspecified"));
    out.push_str(&format!("Reason: {reason}\n"));
    out.push_str("Notice: No skills recommended for this turn.\n");
    out
}

fn render_unavailable_view(val: &Value) -> String {
    let mut out = String::new();
    let code = val["error"]["code"].as_u64().unwrap_or(1);
    let kind = sanitize_terminal_text(val["error"]["kind"].as_str().unwrap_or("unavailable"));
    let message = sanitize_terminal_text(val["error"]["message"].as_str().unwrap_or(""));
    let hint = sanitize_terminal_text(val["error"]["hint"].as_str().unwrap_or(""));

    out.push_str(&format!("UNAVAILABLE (exit {code})\n"));
    out.push_str("========================\n");
    out.push_str(&format!("Error Kind: {kind}\n"));
    out.push_str(&format!("Message:    {message}\n"));
    if !hint.is_empty() {
        out.push_str(&format!("Hint:       {hint}\n"));
    }

    if let Some(unres) = val["unresolved"].as_array()
        && !unres.is_empty()
    {
        out.push_str("\nUnresolved Explicit Requirements:\n");
        out.push_str("  REFERENCE                       REASON\n");
        out.push_str("  ------------------------------  ----------\n");
        for u in unres {
            let reference = sanitize_terminal_text(u["reference"].as_str().unwrap_or(""));
            let reason = sanitize_terminal_text(u["reason"].as_str().unwrap_or(""));
            let c_ref = pad_to_width(&truncate_to_width(&reference, 30), 30, true);
            out.push_str(&format!("  {c_ref}  {reason}\n"));
        }
    }

    out
}

fn render_artifact_view(val: &Value) -> String {
    let mut out = String::new();
    let kind = val["kind"].as_str().unwrap_or("artifact");
    let run_status = val["run_status"].as_str().unwrap_or("unknown");
    let gate_status = val["gate_status"].as_str().unwrap_or("unknown");

    out.push_str(&format!("ARTIFACT: {}\n", kind.to_uppercase()));
    out.push_str("==================\n");
    out.push_str(&format!("Run Status:  {run_status}\n"));
    out.push_str(&format!("Gate Status: {gate_status}\n"));
    out
}

fn render_trace_section(trace_val: &Value) -> String {
    let mut out = String::new();
    let empty_vec = Vec::new();
    let entries = trace_val["entries"].as_array().unwrap_or(&empty_vec);
    if entries.is_empty() {
        return out;
    }

    out.push_str("\nSTAGE TRACE (WHY-NOT / EXPLANATION)\n");
    out.push_str("===================================\n");

    let mut id_width = 8usize; // min "SKILL ID"
    let mut reason_width = 6usize; // min "REASON"
    for e in entries {
        let clean_id = sanitize_terminal_text(e["skill_id"].as_str().unwrap_or(""));
        let clean_reason = sanitize_terminal_text(e["reason"].as_str().unwrap_or("-"));
        id_width = id_width.max(str_width(&clean_id));
        reason_width = reason_width.max(str_width(&clean_reason));
    }
    id_width = id_width.min(24);
    reason_width = reason_width.min(24);

    let stage_width = 16usize; // "quill-admission" is 15
    let status_width = 15usize; // "not-in-snapshot" is 15
    let val_width = 10usize;
    let thresh_width = 10usize;

    let h_id = pad_to_width("SKILL ID", id_width, true);
    let h_stage = pad_to_width("STAGE", stage_width, true);
    let h_status = pad_to_width("STATUS", status_width, true);
    let h_val = pad_to_width("VALUE", val_width, false);
    let h_thresh = pad_to_width("THRESHOLD", thresh_width, false);
    let h_reason = pad_to_width("REASON", reason_width, true);

    out.push_str(&format!(
        "{h_id}  {h_stage}  {h_status}  {h_val}  {h_thresh}  {h_reason}\n"
    ));
    out.push_str(&format!(
        "{}  {}  {}  {}  {}  {}\n",
        "-".repeat(id_width),
        "-".repeat(stage_width),
        "-".repeat(status_width),
        "-".repeat(val_width),
        "-".repeat(thresh_width),
        "-".repeat(reason_width)
    ));

    let mut recovery_hints = Vec::new();

    for e in entries {
        let clean_id = sanitize_terminal_text(e["skill_id"].as_str().unwrap_or(""));
        let clean_stage = sanitize_terminal_text(e["stage"].as_str().unwrap_or(""));
        let clean_status = sanitize_terminal_text(e["status"].as_str().unwrap_or(""));
        let clean_reason = e["reason"]
            .as_str()
            .map(sanitize_terminal_text)
            .unwrap_or_else(|| "-".into());

        let val_str = e["value"]
            .as_f64()
            .map(|v| format!("{v:.6}"))
            .unwrap_or_else(|| "-".into());
        let thresh_str = e["threshold"]
            .as_f64()
            .map(|t| format!("{t:.6}"))
            .unwrap_or_else(|| "-".into());

        if let Some(hint) = e["hint"].as_str() {
            let clean_hint = sanitize_terminal_text(hint);
            if !recovery_hints.contains(&clean_hint) {
                recovery_hints.push(clean_hint);
            }
        }

        let c_id = pad_to_width(&truncate_to_width(&clean_id, id_width), id_width, true);
        let c_stage = pad_to_width(
            &truncate_to_width(&clean_stage, stage_width),
            stage_width,
            true,
        );
        let c_status = pad_to_width(
            &truncate_to_width(&clean_status, status_width),
            status_width,
            true,
        );
        let c_val = pad_to_width(&val_str, val_width, false);
        let c_thresh = pad_to_width(&thresh_str, thresh_width, false);
        let c_reason = pad_to_width(
            &truncate_to_width(&clean_reason, reason_width),
            reason_width,
            true,
        );

        out.push_str(&format!(
            "{c_id}  {c_stage}  {c_status}  {c_val}  {c_thresh}  {c_reason}\n"
        ));
    }

    if !recovery_hints.is_empty() {
        out.push_str("\nRecovery Hints:\n");
        for hint in recovery_hints {
            let desc = match hint.as_str() {
                "inspect-roster" => {
                    "Inspect roster discovery paths and SKILL.md frontmatter syntax."
                }
                "inspect-precedence" => {
                    "Inspect roster precedence: a higher-priority skill shadows this binding."
                }
                "verify-adapter-visibility" => {
                    "Verify adapter visibility contract for this harness."
                }
                "request-explicitly" => {
                    "Skill is manual-only: request explicitly via --require-skill or user prompt."
                }
                "check-invocation-restrictions" => {
                    "Check invocation restrictions: agent invocation is forbidden."
                }
                "review-exclusions" => "Review local policy exclusions: skill is in excluded list.",
                "refine-request" => "Refine user prompt or task anchor to match skill intent.",
                _ => "Inspect configuration and candidate requirements.",
            };
            out.push_str(&format!("  * {hint}: {desc}\n"));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_char_and_string_width_calculations() {
        // ASCII printable
        assert_eq!(char_width('a'), 1);
        assert_eq!(char_width('Z'), 1);
        assert_eq!(char_width(' '), 1);
        assert_eq!(str_width("hello world"), 11);

        // Control characters
        assert_eq!(char_width('\0'), 0);
        assert_eq!(char_width('\x1b'), 0);
        assert_eq!(char_width('\n'), 0);

        // East Asian Wide / CJK
        assert_eq!(char_width('你'), 2);
        assert_eq!(char_width('好'), 2);
        assert_eq!(char_width('コ'), 2);
        assert_eq!(char_width('ー'), 2);
        assert_eq!(char_width('ド'), 2);
        assert_eq!(str_width("你好"), 4);
        assert_eq!(str_width("Code コード"), 11);

        // Emoji
        assert_eq!(char_width('🚀'), 2);
        assert_eq!(char_width('🦀'), 2);
        assert_eq!(str_width("Rust 🦀"), 7);

        // Combining mark (accents)
        assert_eq!(char_width('\u{0300}'), 0); // Combining grave accent
        assert_eq!(str_width("e\u{0300}"), 1); // e + grave accent
    }

    #[test]
    fn test_ansi_and_control_sanitization() {
        let malicious_cpi = "\x1b[31;1mEvil Skill\x1b[0m";
        assert_eq!(sanitize_terminal_text(malicious_cpi), "Evil Skill");

        let malicious_osc = "\x1b]8;;http://malicious.example.com\x07Click Me\x1b]8;;\x07";
        assert_eq!(sanitize_terminal_text(malicious_osc), "Click Me");

        let cursor_tampering = "\x1b[2J\x1b[HSystem Compromised";
        assert_eq!(
            sanitize_terminal_text(cursor_tampering),
            "System Compromised"
        );

        let tabs_and_controls = "Skill\t1\x00\x07Name";
        assert_eq!(sanitize_terminal_text(tabs_and_controls), "Skill 1  Name");
    }

    #[test]
    fn test_padding_and_truncation() {
        let text = "Rust 🦀"; // width 7
        let padded = pad_to_width(text, 10, true);
        assert_eq!(str_width(&padded), 10);
        assert_eq!(padded, "Rust 🦀   ");

        let right_padded = pad_to_width("42", 5, false);
        assert_eq!(str_width(&right_padded), 5);
        assert_eq!(right_padded, "   42");

        let cjk = "你好世界"; // width 8
        let trunc = truncate_to_width(cjk, 5); // fits "你好" (width 4); "世" needs 2 more (total 6 > 5)
        assert_eq!(trunc, "你好");
        assert_eq!(str_width(&trunc), 4);
    }
}
