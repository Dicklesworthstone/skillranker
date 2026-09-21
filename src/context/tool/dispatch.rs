//! Resolve load evidence without letting registered skill names replace the
//! semantics of the harness's loader and file-read tools. This boundary never
//! opens a path or treats ambiguous argument text as a successful load.

use super::{SimpleSkillResolver, SkillEvidenceResolver, SkillMatch};
use crate::limits::NORMALIZED_CONTEXT_JSON_BYTES;
use serde_json::{Map, Value};

pub(super) fn resolve_tool(
    resolver: &SimpleSkillResolver,
    tool_name: &str,
    arguments_json: Option<&str>,
) -> Option<SkillMatch> {
    // Dispatch reserved tools BEFORE direct registered names. A skill named
    // "Read" must not turn every unrelated file read into its own load; a skill
    // named "Skill" remains callable via Skill({"skill":"Skill"}). A reserved
    // tool with invalid or unknown arguments never falls back to a direct name.
    if ["skill", "load_skill", "run_skill"]
        .iter()
        .any(|name| tool_name.eq_ignore_ascii_case(name))
    {
        let arguments = load_arguments(arguments_json)?;
        let target = unique_target(&arguments, &["skill", "skill_name", "name"])?;
        let mut matched = resolver.tool_skills.get(target)?.clone();
        if arguments
            .get("args")
            .is_some_and(|args| !args.is_null() && args.as_str() != Some(""))
        {
            // Invocation-specific arguments invalidate any reusable rendered
            // version registered for a parameterless invocation.
            matched.has_dynamic_arguments = true;
            matched.rendered_content = None;
        }
        return Some(matched);
    }

    if ["read_file", "view_file", "cat", "read"]
        .iter()
        .any(|name| tool_name.eq_ignore_ascii_case(name))
    {
        let arguments = load_arguments(arguments_json)?;
        let target = unique_target(&arguments, &["path", "file_path", "target"])?;
        return resolver.resolve_file_read(target);
    }

    // Adapter-declared direct skill tools retain their existing behavior.
    resolver.tool_skills.get(tool_name).cloned()
}

fn load_arguments(raw: Option<&str>) -> Option<Map<String, Value>> {
    // Use the same bounded, duplicate-key-rejecting decoder as normalized
    // context. Parsing to Value directly would silently pick the last key.
    let value =
        crate::adapter::decode_json(raw?.as_bytes(), NORMALIZED_CONTEXT_JSON_BYTES.max()).ok()?;
    match value {
        Value::Object(arguments) => Some(arguments),
        _ => None,
    }
}

fn unique_target<'a>(arguments: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a str> {
    let mut target = None;
    for key in keys {
        let Some(value) = arguments.get(*key) else {
            continue;
        };
        let name = value.as_str().filter(|name| !name.is_empty())?;
        if target.is_some_and(|previous| previous != name) {
            return None;
        }
        target = Some(name);
    }
    target
}
