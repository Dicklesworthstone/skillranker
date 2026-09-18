# Authoritative Claude Prompt Overlay Contract

Contract specification and verification rules for the Claude `UserPromptSubmit` hook
authoritative prompt overlay, safe transcript file authority, and session isolation.

Satisfies boundary `p3_claude_overlay` (`sr-roadmap-l1i.4.5`).

## Invariants and Semantics

### 1. Authoritative Stdin Prompt
- For Claude Code `UserPromptSubmit`, the stdin `prompt` is authoritative for the new request.
- The prompt may not yet appear in the transcript file on disk (because Claude invokes hooks before or during transcript append).
- The prompt is overlaid into the session history and set as `current_request`. It appears exactly once in the resulting context.

### 2. Deduplication by Event Identity, Never Text Equality Alone
- When the transcript already contains the prompt event (e.g. Claude flushed to disk before hook completion), deduplication matches strictly by event identity (`prompt_id`), overlaying the authoritative text in-place.
- Deduplication by text equality alone is strictly forbidden: repeated identical user messages across turns (e.g. a user submitting "run tests" on turn 1 and "run tests" on turn 2) represent distinct turns and are both preserved in history.
- Satisfies E2E case `hook-prompt-overlaid-once` and assertion `authoritative_prompt`.

### 3. Missing First Transcript as `PromptOnly`
- On the very first turn of a new session, the transcript file may not yet exist on disk.
- An absent transcript file is authorized and ranked with the hook prompt and an empty transcript, recording `context_quality: prompt_only`.

### 4. Malformed Existing Transcript Rejection
- If a transcript file exists on disk but contains corrupted or malformed records, it must **never** silently fall back to an empty history or prompt-only mode.
- Any existing malformed transcript is rejected with `OverlayError::MalformedTranscript`.

### 5. Transcript Path Confinement and Device Rejection
- A transcript path grants read access exclusively to that specific regular file.
- Directories (`TranscriptIsDirectory`), FIFOs, character/block devices, and Unix domain sockets (`TranscriptIsDeviceOrFifo`) are rejected immediately.
- Broken symlinks or symlinks targeting devices/directories are rejected.
- When an authorized root directory is enforced, transcript paths resolving outside that root are rejected with `OverlayError::CrossSessionReadForbidden`.

### 6. Session and Branch Association
- The hook's `session_id` is bound to the overlay result. Cross-session reads and parent/subagent cursor sharing are forbidden.

## Verification Matrix

| Assertion / Case | Requirement | Verification Location |
|---|---|---|
| `tests/context_contract.rs::claude_prompt_overlay` | Full contract verification across all 9 fixture shapes | `tests/context_contract.rs` |
| `first_file_missing` | Absent first file succeeds as `ContextQuality::PromptOnly` | `tests/context_contract.rs` |
| `prompt_absent` | Absent prompt appended authoritatively once | `tests/context_contract.rs` |
| `prompt_present_dedup` | Present prompt overlaid by `prompt_id` without duplication | `tests/context_contract.rs` |
| `equal_text_distinct_turns` | Identical text across turns preserved as distinct turns | `tests/context_contract.rs` |
| `malformed_rejected` | Malformed existing file rejected; never silently empty | `tests/context_contract.rs` |
| `fifo_rejected` | FIFOs rejected immediately | `tests/context_contract.rs` |
| `directory_rejected` | Directories rejected immediately | `tests/context_contract.rs` |
| `cross_session_rejected` | Paths outside authorized root rejected | `tests/context_contract.rs` |
| `symlink_handling` | Valid regular symlink allowed; broken symlink rejected | `tests/context_contract.rs` |
| `hook-prompt-overlaid-once` | E2E case: prompt overlaid exactly once | `tests/context_contract.rs` |
| `authoritative_prompt` | Assertion ID: hook prompt authoritative for new request | `tests/context_contract.rs` |
