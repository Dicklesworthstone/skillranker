# Strict CLI and Provenance-Aware Configuration Contract

Satisfies roadmap task `sr-roadmap-l1i.5.1` and contract boundary `p4_strict_cli`.

## 1. Boundary Purpose and Scope

`sr` exposes a strict, provenance-aware command-line interface. All inputs, options, and flags
are validated at process entry before any discovery, network transmission, or state creation
occurs. Invalid usage or malformed configuration fails closed with standardized error kinds and
exit codes.

## 2. Invariants and Guarantees

### Strict Flag and Argument Parsing
- **Mutual Exclusivity**:
  - Format flags: `--json` and `--table` are mutually exclusive.
  - Effect flags: `--offline` and `--allow-network` are mutually exclusive.
  - Subcommands: Top-level flags (`--help`, `--version`) cannot accompany subcommands.
  - Redundant arguments: Duplicate flags (e.g. `--top 1 --top 2`) are rejected.
- **Fail-Closed Unknowns**:
  - Unknown commands or subcommands exit with code 2 (`invalid-usage`).
  - Unknown options or CLI flags exit with code 2 (`invalid-usage`).
  - Unknown keys in configuration files exit with code 2 (`invalid-configuration`).

### Precedence Hierarchy and Provenance Tracking
Configuration resolves along a strict, non-widening hierarchy:
```text
built-in defaults → trusted user config → project config → environment variables → CLI flags
```
Each key's effective value is tagged with its authoritative source layer (`built-in`, `trusted-user`,
`project`, `environment`, `cli`).

### Privacy and Canary Non-Disclosure
- Credentials (`TYPESAFE_API_KEY`) and secret tokens are never printed in plaintext in stdout,
  stderr, or JSON report envelopes.
- Error messages never echo invalid input or secret tokens.
- Symlinks escaping the workspace root are strictly rejected to prevent path-traversal disclosures.

### Zero-Side-Effect Local Inspection
- Command inspection (such as `sr doctor --config` and `sr --help`) creates zero filesystem mutations,
  zero persistent state files, zero database locks, and spawns zero child processes.

### Consequential Boundary Re-reads
- `ConfigFiles::refresh` re-reads mutable file layers (`trusted-user` and `project`) against the
  monotonic `EntryClock`, while preserving validated invocation CLI and environment overrides.
- Returns `Revalidation::Unchanged` for identical or shadowed/overridden edits.
- Returns `Revalidation::Superseded(fields)` when consequential fields (such as network consent) change.
- Rejects newly malformed configuration with `invalid-configuration` (exit 2), preventing any
  unauthorized pipeline progress.

## 3. Verification Evidence

- Unit Property Test: `tests/cli_contract.rs::strict_flags_and_provenance` (passing).
- Binary Integration: `tests/cli_config.rs` (8/8 tests passing).
- Zero canary leakage verified via synthetic token testing.
