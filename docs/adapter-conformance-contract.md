# Adapter Conformance Contract (P3)

This document formalizes the contract for the versioned native, normalized, and cass adapter conformance suite (`p3_conformance_matrix`, bead `sr-roadmap-l1i.4.13`).

## 1. Scope and Mission

SkillRanker ingests context across three distinct, versioned source adapter boundaries:
1. **Native Claude Hook and JSONL Transcript Adapter**: Ingests authoritative `UserPromptSubmit` hook payloads from stdin alongside incremental JSONL session transcripts from disk.
2. **Normalized Context Input Envelope**: Decodes a complete, validated `NormalizedContext` (v1 schema) containing session metadata, project signals, loaded skills, and normalized event streams.
3. **Cass Archive Subprocess Adapter**: Optionally queries a locally verified `cass` installation to extract archived session events without granting shell or network authority.

The purpose of the conformance suite is to ensure that:
1. Corrupted, oversized, hostile, or malformed inputs are safely rejected at the boundary with sanitized diagnostics (`assertion_id: safe_context_rejection`).
2. Input adapters enforce hard resource limits before parsing or allocating unbounded data.
3. Session identity, branches, and workspaces remain strictly isolated with zero cross-session or cross-producer conflation.
4. An honest, fully verified success counterpart exists for every failure mode, proving that conformant sessions are accepted without false rejections.

## 2. Invariants

### Invariant 1: Safe Context Rejection on Corrupted Input (`corrupted-jsonl`)
- **Native Transcripts**: If an existing transcript file contains invalid JSON syntax, truncated records, or unclosed objects, the reader must reject the file with `OverlayError::MalformedTranscript` (`SkipKind::Corrupt`). It must **never** silently swallow the error or pretend the session has an empty history.
- **Normalized Envelopes**: Corrupted JSON bytes must be rejected with `ContextError::InvalidJson` before deserialization.
- **Cass Archives**: Malformed JSON responses from `cass export` must be rejected with `CassError::InvalidResponse`.
- In all cases, diagnostics must be sanitized: they must never echo raw line contents, sensitive tokens, or unredacted prompt text.

### Invariant 2: Strict Enforcement of Record and Payload Bounds (`oversized-record`)
- **Single Transcript Record**: Bounded to `ONE_TRANSCRIPT_RECORD_BYTES` (256 KiB). Lines exceeding this threshold must be rejected with `SkipKind::Oversize` / `OverlayError::MalformedTranscript("transcript record exceeds byte limit")`.
- **Normalized Context Payload**: Bounded to `NORMALIZED_CONTEXT_JSON_BYTES` (1 MiB). Envelopes exceeding this limit must be rejected with `ContextError::LimitExceeded`.
- **Cass Output**: Bounded to `CASS_STDOUT_BYTES` (8 MiB). Exports exceeding this stdout budget must be rejected with `CassError::LimitExceeded`.
- Enforcing limits before full parsing protects the engine against memory exhaustion and denial-of-service vectors.

### Invariant 3: Authoritative Prompt Requirement (`missing-prompt`)
- An active user prompt is required for ranking.
- If a Claude hook payload supplies an empty prompt, null prompt, or whitespace-only string, the adapter must reject the input with `OverlayError::MissingPrompt`.
- It must not guess a prompt from earlier transcript turns or fabricate an empty request.

### Invariant 4: Duplicate Key Rejection
- All JSON decoding across native transcripts and normalized envelopes strictly forbids duplicate object keys.
- Duplicate keys (e.g., `{"role":"user","role":"assistant"}`) must result in `SkipKind::DuplicateKey` / `OverlayError::MalformedTranscript` in transcripts, and `ContextError::DuplicateKey` in normalized envelopes.
- Duplicate entity identifiers (event IDs, supplied skill IDs) within a single envelope are rejected with `ContextError::DuplicateEvent` or `ContextError::DuplicateLoadDefinition`.

### Invariant 5: Filesystem Confinement and Path Safety
- Transcript file paths must resolve to a regular, authorized file:
  - Directories are rejected with `OverlayError::TranscriptIsDirectory`.
  - Named pipes (FIFOs), character devices, and block devices are rejected with `OverlayError::TranscriptIsDeviceOrFifo`.
  - Traversal attempts escaping the workspace `authorized_root` are rejected with `OverlayError::TranscriptPathForbidden`.
  - Symlinks to non-regular files or dangling paths are rejected by `snapshot_jsonl` with `JsonlError::UnsafePath`.
- Readers snapshot file length, read complete lines, and defer an incomplete trailing line (`incomplete_tail`) without corrupting previously read records.

### Invariant 6: Strict Session and Namespace Isolation
- **Claude Overlay**: The `session_id` declared in the hook payload must match any session identifier recorded in the transcript. A mismatch is rejected with `OverlayError::SessionMismatch`. Cross-session reading is forbidden (`OverlayError::CrossSessionReadForbidden`).
- **Source Provenance**: Each adapter generates a distinct `SourceProvenance` discriminant:
  - `SourceProvenance::NativeTranscript`
  - `SourceProvenance::Normalized`
  - `SourceProvenance::Cass`
- Sessions, events, and worktrees from different provenance domains cannot be merged, aliased, or substituted for one another.

### Invariant 7: Policy and Capability Conformance for Cass
- **Policy Gates**: `CassAdapter` can only be invoked when `SourcePolicy::cass_allowed()` is true. It is strictly disabled under `--offline`, `--dry-run`, or `--local-only` policies (`CassError::UnsupportedSourceMode`).
- **Qualified Version**: Only cass version `0.8.0` (`QUALIFIED_VERSION`) with API version 1 and contract version 1 is supported. Mismatched versions or incomplete feature flags fail with `CassError::UnsupportedProducer`.
- **Local Sources Only**: `ArchiveSelection::local` strictly requires `source_id == "local"`. Remote sources (e.g., cluster endpoints, network shares) are rejected with `CassError::RemoteSource`.
- **Tool Filtering**: `decode_export` validates records and projects clean text streams when `include_tools == false`, dropping raw tool blocks and functions without relying on external trust.

### Invariant 8: Honest Success Counterparts
- For every failure or refusal boundary, an honest counterpart verifies that conformant inputs are accepted:
  - First-turn session (where transcript does not exist on disk yet) succeeds with `ContextQuality::PromptOnly`.
  - Multi-turn native transcripts with prompt overlay parse events, associate tools, and deduplicate by event ID.
  - Well-formed normalized envelopes parse into valid `NormalizedContext` with intact provenance and signals.
  - Well-formed cass exports decode into valid `ArchiveExport` with tool filtering intact.

## 3. Conformance Matrix & Verification Mapping

| Test Case | Adapter | Input Condition | Expected Result | Assertion ID |
|---|---|---|---|---|
| `corrupted-jsonl` | Native / Claude | Malformed JSON syntax in transcript | `OverlayError::MalformedTranscript` / `SkipKind::Corrupt` | `safe_context_rejection` |
| `oversized-record` | Native / Claude | Line length > 256 KiB | `OverlayError::MalformedTranscript` / `SkipKind::Oversize` | `safe_context_rejection` |
| `missing-prompt` | Claude Hook | Empty / whitespace prompt string | `OverlayError::MissingPrompt` | `safe_context_rejection` |
| `duplicate-key` | Native & Normalized | Repeated key within object | `MalformedTranscript` / `ContextError::DuplicateKey` | `safe_context_rejection` |
| `path-directory` | Claude Hook | Transcript path is a directory | `OverlayError::TranscriptIsDirectory` | `safe_context_rejection` |
| `path-traversal` | Claude Hook | Transcript outside authorized root | `OverlayError::TranscriptPathForbidden` | `safe_context_rejection` |
| `session-mismatch` | Claude Hook | Hook session differs from transcript | `OverlayError::SessionMismatch` | `safe_context_rejection` |
| `schema-unsupported`| Normalized | `schema_version != 1` | `ContextError::UnsupportedSchema` | `safe_context_rejection` |
| `cass-offline` | Cass | `SourcePolicy { offline: true, .. }`| `CassError::UnsupportedSourceMode` | `safe_context_rejection` |
| `cass-remote` | Cass | Non-local archive source | `CassError::RemoteSource` | `safe_context_rejection` |
| `cass-bad-version` | Cass | Version != `0.8.0` | `CassError::UnsupportedProducer` | `safe_context_rejection` |
| `native-first-turn` | Native / Claude | Missing file on first prompt | Ok(`ContextQuality::PromptOnly`) | `context_accepted` |
| `native-multiturn` | Native / Claude | Valid JSONL transcript + prompt | Ok(`events.len() > 1`, prompt overlaid) | `context_accepted` |
| `normalized-valid` | Normalized | Valid v1 envelope | Ok(`NormalizedContext`, valid definitions) | `context_accepted` |
| `cass-export-valid`| Cass | Valid capabilities + export JSON | Ok(`ArchiveExport`, text-only filtered) | `context_accepted` |
