# Skill Metadata and Frontmatter Contract

## 1. Scope and Mission

SkillRanker discovers and indexes skills from visible workspace and user roots.
Each skill is defined by a markdown file (typically `SKILL.md`).
This document specifies the parsing rules, resource limits, fallback semantics,
and security guardrails for skill frontmatter and metadata parsing (`sr-roadmap-l1i.3.3`).

## 2. Resource Budgets and Limits

To prevent unbounded file reads, memory exhaustion, and denial of service:
- **Maximum File Size**: 256 KiB (`MAX_SKILL_FILE_BYTES`). Files exceeding this size fail with `FrontmatterError::FileTooLarge`.
- **Maximum Frontmatter Size**: 16 KiB (`MAX_FRONTMATTER_BYTES`). Frontmatter blocks exceeding this limit fail with `FrontmatterError::FrontmatterTooLarge`.
- **Maximum Frontmatter Nesting Depth**: 8 (`MAX_FRONTMATTER_DEPTH`).
- **Wide Description Limit**: 160 Unicode scalar values (`WIDE_DESCRIPTION_MAX_SCALARS`).
- **Rerank Description Limit**: 1,000 Unicode scalar values (`RERANK_DESCRIPTION_MAX_SCALARS`).
- **Body Excerpt Limit**: 700 Unicode scalar values (`BODY_EXCERPT_MAX_SCALARS`).

## 3. Frontmatter Delimiters and Encoding

- **Encoding**: UTF-8. UTF-8 Byte Order Marks (BOM `\u{feff}`) are detected and stripped transparently.
- **Line Endings**: Both CRLF (`\r\n`) and LF (`\n`) are supported transparently.
- **Opening Delimiter**: `---` on the first line (or first non-empty line after leading whitespace or BOM).
- **Closing Delimiter**: `---` or `...` on its own line.
- Unclosed frontmatter blocks return `FrontmatterError::UnclosedFrontmatter`.

## 4. Fallback Behavior for Missing Frontmatter

If a document does not begin with frontmatter (`---`), it is treated as a valid skill with missing frontmatter:
1. **Title Fallback**: The first top-level Markdown heading (`# <Title>`) outside fenced code blocks becomes the skill name.
2. **Description Fallback**: The first non-empty paragraph under the title (or at file start) outside code fences becomes the description.
3. **Parse Warning**: `ParseWarning::MissingFrontmatter` is recorded in the metadata.
4. Missing frontmatter is a normal condition and does **not** cause parse failure.

## 5. YAML Safety and Parsing Invariants

SkillRanker uses a bounded, pure-Rust parser for frontmatter with strict security controls:
- **Duplicate Key Rejection**: Frontmatter containing duplicate keys (e.g. two `name:` entries) is strictly rejected with `FrontmatterError::DuplicateKey`. No last-key-wins behavior is permitted.
- **No Anchors or Aliases**: YAML anchors (`&anchor`) and aliases (`*alias`) are strictly forbidden to prevent algorithmic complexity and memory expansion attacks (e.g., billion laughs). Documents with aliases return `FrontmatterError::AliasForbidden`.
- **Inert Substitutions**: Command substitutions (`!cmd`, `` `cmd` ``, `$(cmd)`), shell variables (`${VAR}`), and template placeholders (`{{arg}}`) are treated strictly as inert plain text. They are never evaluated or passed to subshells.
- **Sanitized Diagnostics**: Parse errors never retain or echo raw YAML input lines or confidential tokens.

## 6. Supported Frontmatter Fields

| Field | Type | Description | Default |
|---|---|---|---|
| `name` | String | Callable or display name of the skill | Falls back to H1 title |
| `description` | String / Block | Full description of skill capability (supports literal `\|` and folded `>` multiline blocks) | Falls back to first paragraph |
| `disable-model-invocation` | Boolean | If `true`, the model is not permitted to automatically invoke this skill | `false` |
| `user-invocable` | Boolean | If `false`, the user cannot manually invoke the skill | `true` |
| `usage` | String | Skill usage kind: `reference`, `workflow`, or `unknown` | `unknown` |
| `aliases` | List[String] | List of alternate callable or legacy invocation names | `[]` |
| `tags` | List[String] | Categorization tags for retrieval | `[]` |
| `phases` | List[String] | Declared workflow phases | `[]` |

## 7. Code Fence State and Markdown Isolation

Markdown parsing respects fenced code blocks (` ``` ` and `~~~`):
- Start-of-line hashes (`# ` / `## `) appearing inside fenced code blocks (e.g., shell comments or Python code) are treated as code lines, **not** markdown headings.
- They do not create false skill titles or prematurely terminate the description paragraph.
