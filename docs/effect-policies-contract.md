# Independent Persistence, Offline, and Dry-Run Effect Policies Contract

Satisfies roadmap task `sr-roadmap-l1i.5.9` and contract boundary `p4_effect_policies`.

## 1. Boundary Purpose and Scope

`sr` enforces independent, non-interfering controls for persistence, networking, and execution modes.
Invocation effect flags (`--offline`, `--allow-network`, `--dry-run`, `--no-cache`, `--no-ledger`,
`--no-persist`, `--save-case`) govern store access, network transmission, and state mutability.
Flags combine by taking the strictly more restrictive behavior.

## 2. Invariants and Guarantees

### Persistence Control Matrix
| Mode | Response Cache | Ledger & Observation Cursors | Coordination / Key State |
|---|---|---|---|
| **Default** | Read / write | Read / write when initialized | Bounded local leases, cooldowns, and owner-only hash key |
| `--no-cache` | Disabled (`Restriction::NoCache`) | Unchanged (Read / write) | Unchanged; coordination stores no response bodies |
| `--no-ledger` | Unchanged (Read / write) | Disabled (`Restriction::NoLedger`); reconstruct transient evidence | Unchanged |
| `--no-persist` | No disk reads/writes (`Restriction::NoPersist`) | No disk reads/writes (`Restriction::NoPersist`) | In-memory only; no persistent key/lease/cooldown access |
| `--dry-run` | No disk reads/writes (`Restriction::DryRun`) | No disk reads/writes (`Restriction::DryRun`) | Stateless preview; implies `--no-persist` |

### Filesystem and State Protection
- "No disk reads/writes" strictly applies to `sr`'s persistent state stores (cache database, observation
  ledger, coordination locks, and runtime state keys).
- Ordinary configuration files (trusted user config and project config) and session inputs remain
  fully readable in all modes, including `--no-persist` and `--dry-run`.
- Under disabled persistence, `open_cache` returns `CacheOpen::Disabled` immediately without creating
  or touching any SQLite database file, journal, lock, sidecar, or migration.

### Network Admission and Refusal Matrix
- Network consent is checked **before** credential presence in `admit_provider_attempt`.
- **Offline mode** (`--offline`):
  - Guarantees zero network calls.
  - Refusal returns `ProviderAdmissionRefusal::Offline`, mapped to `ErrorKind::CacheMiss` (exit code 11),
    never an authentication failure, even if `TYPESAFE_API_KEY` is missing.
- **Dry-run mode** (`--dry-run`):
  - Guarantees zero network calls and zero persistent state mutations.
  - Refusal returns `ProviderAdmissionRefusal::DryRun`, mapped to `ErrorKind::NetworkDenied` (exit code 8).
- **Default mode** (no explicit network consent):
  - Refusal returns `ProviderAdmissionRefusal::NetworkNotAuthorized` (`ErrorKind::NetworkDenied`, exit code 8).
- **Authorized mode** (`--allow-network`):
  - With credential present: succeeds with `ConsentSource::AllowNetworkFlag`.
  - Without credential: fails with `ProviderAdmissionRefusal::MissingCredential` (`ErrorKind::Authentication`, exit code 4).

### Subprocess / Cass Restriction
- Under `--offline`, `--dry-run`, or `--local-only`, external discovery or cass subprocesses are strictly forbidden.
- Explicitly requesting a cass session source returns `SourceError::CassUnavailableInMode`, mapped to
  `ErrorKind::UnsupportedSourceMode` (exit code 7) with the actionable hint:
  `"Choose a direct transcript or normalized context in this mode."`

### Explicit Local Requests
- Explicit requirements (`--require-skill`, structured directives) resolve locally without requiring
  network access or provider admission, executing reliably in offline mode.

### Honest Missing Evidence
- Missing persistence removes historical evidence rather than fabricating empty negative observations.

## 3. Verification Evidence

- Unit & Property Test: `tests/effect_policies.rs` (6/6 tests passing):
  - `effect_policy_matrix_128_combinations`: all 128 bit combinations tested, 52 valid states verified, 76 conflict states caught.
  - `persistence_controls_prevent_disk_store_creation`: zero database or journal files created under disabled cache.
  - `ordinary_config_reads_permitted_in_all_modes`: user/project configs load and refresh under `--no-persist`, `--dry-run`, `--no-cache`, and `--no-ledger`.
  - `network_admission_and_refusal_matrix`: exact refusal reasons and exit codes verified across offline, dry-run, unauthorized, and authorized flows.
  - `offline_and_dry_run_cass_restriction_yields_error_7`: exit code 7 verified for cass under offline, dry-run, and local-only modes.
  - `explicit_local_requests_resolve_offline`: local explicit resolution validated without network admission.
