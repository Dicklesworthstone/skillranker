# Exact source selection

`context::source` binds one local input before a reader runs. It neither reads
stdin speculatively nor selects a source by inspecting JSON shape. The library
boundary supplies selection to the normalized, native and cass readers; it does
not enable the later `rank` or hook CLI commands by itself.

## Explicit inputs

`SourceOptions::resolve` accepts a separately resolved `WorkspaceId`, source
effect policy, controlling-terminal availability and a lazy discovery callback.
The caller supplies its observation of offered stdin in `stdin_supplied`.
Offered stdin without an explicit hook/normalized-stdin mode is a usage error
before discovery, including when a file source was also supplied. The selector
never probes a pipe or guesses its format. A noninteractive invocation with no
offered stdin can still discover a unique session.
The callback is called only when no explicit source was selected. Source flags
are mutually exclusive; `latest` cannot accompany an explicit source.

| Options | Bound reader target |
|---|---|
| `claude_hook` | Claude hook stdin, whose decoded event supplies exact identity |
| `context = "-"` | Normalized stdin, even if empty or attached to a terminal |
| `context = FILE` | That normalized file |
| `transcript = FILE`, `harness = claude_code` | That Claude native transcript |
| `cass_session = PATH` | That cass archive session |
| No source | Bounded discovery for the exact resolved workspace |

An absent native harness is a usage error. An unsupported harness is
`unsupported-input` (exit 7); the selector does not infer a format from a path.
Native/cass bare `-` is rejected; stdin is available only through the documented
hook or normalized mode. Paths retain native bytes and have private Debug output.
They must be nonempty, NUL-free and at most 4 KiB. Selection does not grant file
access: regular-file checks, authorized roots, symlink containment, snapshots and
byte/depth limits belong to the readers.

`SourceSelection::read` invokes the supplied reader once and returns its result
unchanged. It has no fallback or retry path. A missing, malformed or replaced
chosen source never triggers discovery or opens another conversation. The reader
must compare a discovered selection's `expected_identity` with the opened
source; a selection is not proof that the filesystem stayed unchanged.

## Discovery and identity

Discovery receives `SourcePolicy` before doing work. Its adapter must enforce
that policy at its effect boundary, bound enumeration/parsing before allocation,
and return a complete inventory or an explicit failure. The selector also bounds
the inventory to 10,000 entries and 32 MiB of path bytes before copying candidates.
Incomplete pagination or an entry with unresolved workspace identity prevents a
uniqueness claim. No missing attribution is invented.

Only exact workspace-ID equality admits an entry. Prefix matches, ancestors and
a linked worktree's shared Git directory confer no relationship. The trusted
resolver must identify each worktree separately; this module does not implement
that filesystem resolver. Session, agent, branch, epoch and source provenance are
preserved. Duplicate identities fail even when their paths or timestamps differ.
Native and cass provenance must match the selected reader. Normalized imports
are explicit inputs and never masquerade as discovered native sessions.

| Complete eligible inventory | Result |
|---|---|
| Empty | `missing-session`, exit 3 |
| One entry | Select it with `UniqueInWorkspace` |
| Several, no controlling terminal | `ambiguous-session`, exit 3 |
| Several, controlling terminal | `NeedsChoice`; UI must obtain an explicit index |
| Explicit latest request | Select a uniquely newest timestamp with `LatestRequested` |

Latest selection requires known timestamps and a unique maximum. Ties or unknown
recency fail as ambiguous, including in a TTY. Its reason and eligible candidate
count remain available for disclosure; it never claims that recency proves a
session is live. Interactive choices retain an immutable candidate inventory;
out-of-range choices fail. UI rendering and terminal reads are separate effects.

Remote candidates in the selected workspace are rejected. Cass `source_id`,
when present in discovered provenance, remains attached to the chosen session.
An explicit cass path has no invented source ID: its reader must obtain and
validate provenance from the actual export.

## Source policy and diagnostics

Offline, dry-run and local-only modes refuse explicit cass before discovery or
reader invocation: `unsupported-source-mode`, exit 7, with a direct/normalized
input hint. A discovery adapter must avoid invoking cass under those policies;
the selector also refuses an accidentally supplied eligible cass entry instead
of silently picking a native alternative. `allow_network` conflicts with these
local modes. Selection never reads credentials or authorizes Jev transmission.

Source errors contain fixed diagnostic text and map to the existing output
`ErrorKind` contract. Private paths, unknown harness names and session IDs are
not included in error text or Debug output.

## Verification boundary

`tests/source_selection.rs` exercises all explicit routes, conflicting flags,
stdin-shape confusion, mode restrictions, concurrent sessions, subagent branches,
latest disclosure, incomplete/duplicate/remote inventories and private diagnostics.
The filesystem tests create real files and linked Git worktrees, read the bound
file, and retain failed-source/successful-neighbor counterparts. Their synthetic
contents are selection evidence, not native-parser or live-harness evidence.

The P3 `context` end-to-end suite remains planned in the phase matrix. Concrete
Rust test references replace the earlier prospective `context_contract.rs` name;
executed checks are recorded on bead `sr-roadmap-l1i.4.1`. Native snapshot parsing,
normalized decoding, cass capability/subprocess integration and CLI delivery
remain their respective downstream tasks. No live Jev request is needed to test
this local boundary.
