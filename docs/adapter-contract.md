# Adapter and capability contracts (P0)

This boundary freezes first-supported source kinds, the native-advice gate, and
the capabilities document. It does not implement Claude or cass adapters, read
transcripts, start daemons, install hooks, or emit `additionalContext`.

`capabilities --json` is a later CLI command. The library type already
distinguishes implemented CLI (`help`, `version`) from planned commands and
their earliest gated phase. Planned names are not implemented support.

## First-supported sources

| Kind | Adapter ID | Default hook path | Foundation support |
| --- | --- | --- | --- |
| Normalized local envelope | `normalized` | no | Implemented types; input may be accepted |
| Claude `UserPromptSubmit` | `claude_code` | yes | Unverified; official schema is not a smoke test |
| Optional cass archive | `cass` | no | Unverified; remote sources are rejected |

Native Codex/omp/Grok adapters remain unavailable. A related harness version
cannot inherit a tested support cell.

## Advice gate

`AdapterRecord::advice` answers two questions:

- `AcceptInput` — may this source be parsed as local input
- `EmitNativeAdvice` — may a hook inject advice for an installed version

Native advice requires `support = tested`, the installed version in
`tested_versions`, compatible identity and visibility semantics, and **each**
of prompt timing, branch identity, visibility, restrictions, compaction, load
evidence, hook output, deadline, and delivery marked `pass` with a passing
real-harness smoke record for that version. Official-schema qualification,
local CLI version observation, and fixture digests are retained as provenance;
none of them authorizes native advice. `transfer_tested_support` rechecks this
entire gate for the same adapter and installed version. Tested and unverified
version lists must each be unique and cannot overlap. Both document validation
and direct `AdapterRecord::advice` calls check the entire definition, including
versions other than the installed version; an invalid directly constructed record
returns `InvalidVersionDefinitions` and cannot transfer tested support. Cass is
never the default hook path and cannot authorize native injection by claiming
that flag.

Unknown or unsupported capability schema versions are refused. Unknown hook
events are refused. `UserPromptExpansion` is a known non-advisory event.

## Envelopes

Claude stdin is bounded by `hook_stdin` (1 MiB) and nesting depth 64. Duplicate
object keys fail. Required fields are `hook_event_name` and `prompt`. `prompt_id`
is optional event identity; its absence is unknown, not text-equality. Unknown
keys follow `UnknownFieldPolicy`: SkillRanker-owned documents reject them;
foreign harness envelopes may retain them as additive data that cannot change
identity or visibility. `additionalContext` is at most 1,024 Unicode scalar
values and at most one suggested invocation name. Oversized text is rejected
after counting at most 1,025 scalars, using the shared resource limit.

Optional `transcript_path` and `cwd` accept strings, null, or absence. Present
non-string values are errors rather than missing-source fallbacks. Retained
additive keys and values are private; debug formatting reports only their count.

A missing first transcript is `HookTranscriptState::Missing` (`prompt_only` at
the later output boundary). A malformed existing transcript is an error, not an
empty history.

`select_source` preserves declared source modes independently of whether stdin
is currently available. Explicit normalized stdin remains stdin even without a
detected pipe; the later reader reports empty or invalid input. An explicit stdin
marker requires normalized-context or hook mode and cannot silently select
discovery, a native transcript, or cass. Conflicting source flags are rejected
before checking whether unsolicited piped input lacks an explicit mode.

Cass JSON exports remain native/archive shapes and omit skills by default. They
are not a SkillRanker normalized envelope. Archive identity may omit
`build_commit`; a support claim still needs a clean commit ID or a binary digest.
Accepted commit labels are cass's 12-hex-digit abbreviation or full 40/64-hex-digit
Git IDs, optionally suffixed with `-dirty`, and the literal `unknown` used by
builds without Git metadata. Dirty and unknown labels remain valid archive
metadata but require a binary digest for support claims. Malformed labels are
rejected without echoing them. This validates provenance shape, not authenticity
or harness conformance; those require independent evidence. `CassProducer` is
SkillRanker's local metadata contract, not a direct parser for all cass output.

The fixtures under `tests/fixtures/adapter-*.json` and
`tests/fixtures/capabilities.v1.json` are synthetic. They have slots for later
real-harness evidence and do not prove an installed Claude or cass version.
