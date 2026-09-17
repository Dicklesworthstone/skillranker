# Atomic No-Clobber Export Contract

Contract specification and verification rules for shared, bounded, owner-only
file exports (used by roster snapshots, recorded cases, and Claude hooks).

Satisfies boundary `p2_atomic_export` (`sr-roadmap-l1i.3.14`).

## Invariants and Semantics

### 1. Atomic No-Clobber Publication
- Exports use `renameat2(..., RENAME_NOREPLACE)` on Linux systems (with portable atomic `hard_link` + `unlink` fallback) to publish completed files into their final location.
- An existence check followed by an ordinary rename is strictly forbidden because it leaves a race window where another process could create a target that gets overwritten.
- If the target path exists (whether as a regular file, directory, FIFO, or valid/broken symlink), publication fails immediately with `ExportError::TargetAlreadyExists`.
- Even in a high-concurrency race where multiple writers target the exact same file simultaneously, the kernel guarantees that at most one writer succeeds; all other writers fail with `TargetAlreadyExists`, and file contents are never clobbered or truncated.

### 2. Owner-Only Permissions (`0o600`)
- Exported snapshot and case files contain potentially sensitive or private session/metadata information.
- Files are created exclusively with mode `0o600` (`-rw-------`). Group and world permissions are stripped.
- Satisfies `assertion_id = "owner_only_permissions"`.

### 3. Safe Destination Directory Validation
- The destination parent directory must exist and be a valid directory.
- Ownership and permissions are validated against trusted directory policy:
  - Must be owned by the current user with non-group/world-writable permissions, OR
  - Must be a root-owned sticky directory (such as Linux `/tmp` with mode `0o1777`).
- Uncontrolled or group-writable non-sticky directories are rejected with `ExportError::Permissions`.

### 4. File and Directory Durability Flush Policy
- Before publication: the partial file data is committed to disk via `file.sync_all()` (`fsync`).
- After publication: the parent directory is opened and synced via `dir.sync_all()` to ensure that the new directory entry is durable on persistent storage.

### 5. Private Identifiable Partial Files and Cleanup
- Temporary files are written in the same directory as the target (guaranteeing identical filesystem mount point for atomic rename/link).
- Partial files are named using the identifiable pattern:
  `.<target_filename>.sr-partial-<pid>-<nanos>`
- A scope guard pattern (`PartialFileGuard`) guarantees that if writing, syncing, or publication is aborted or interrupted by an error, the temporary file is removed immediately.

### 6. Bounded Limits
- Default maximum size for roster snapshots: 32 MiB (`DEFAULT_MAX_SNAPSHOT_BYTES`).
- Default maximum size for recorded cases: 16 MiB (`DEFAULT_MAX_CASE_BYTES`).
- Content size is validated before initiating writes; oversized payloads fail immediately with `ExportError::Oversized`.

## Verification Matrix

| Assertion / Case | Requirement | Verification Location |
|---|---|---|
| `tests/roster_contract.rs::atomic_private_exports` | Basic export, 0600 mode, no-clobber, symlink rejection, bounds | `tests/roster_contract.rs` |
| `atomic-rename-permissions-0600` | Owner-only 0600 permissions verified on published file | `tests/roster_contract.rs` |
| `owner_only_permissions` | Strict 0600 mode invariant | `tests/roster_contract.rs` |
| `concurrent_export_race_prevents_clobber` | Multithreaded race: exactly 1 winner, 7 collisions, 0 overwrites | `tests/roster_contract.rs` |
