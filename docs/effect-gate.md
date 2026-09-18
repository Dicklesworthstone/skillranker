# Central effect gate (P4)

`skillranker::effects::EffectGate` turns this process's effect flags into
every boundary's restriction. It is built once from the command line
(`EffectGate::new(flags, scope)`), and consumers take their values from it
instead of reading raw flags. This matters because the context source policy
used to be assembled from separate booleans: a caller could pass a default,
cass-permitting policy during an offline run.

| Boundary | Gate method | Disabled by |
| --- | --- | --- |
| Response cache | `response_cache`, `cache_access`, `open_cache` | `--no-cache`, `--no-persist`, `--dry-run` |
| Ledger, ingestion cursors | `ledger`, `history` | `--no-ledger`, `--no-persist`, `--dry-run` |
| Hash key, locks, leases, cooldowns, allowance | `runtime_state`, `cross_process_coordination` | `--no-persist`, `--dry-run` |
| cass and other unverified children | `source_policy`, `unverified_children` | `--offline`, `--dry-run`, local inspection |
| Provider network | `network_consent` | `--offline`, `--dry-run`, local inspection; otherwise trusted consent is required |

Rules:
- Conflicting flags are rejected with every conflict listed, never corrected:
  `--offline`/`--allow-network`, `--dry-run`/`--allow-network`,
  `--save-case`/`--dry-run` and `--save-case`/`--no-persist`.
- Flags combine restrictively, and the reported cause is the strongest flag
  (`dry-run`, then `no-persist`, then the store's own flag).
- `--offline` controls the network only. It does not disable persistence.
- `--dry-run` disables every store, so a preview equals the matching
  `--no-persist` run. `stateless` reports this.
- A disabled ledger makes history `Withheld`, meaning unknown. Callers must
  not turn it into an empty history, zero loads or negative observations.
- A disabled cache returns before resolving its location or inspecting SQLite.
- Configuration and input files are not stores, so they are read in every
  mode. Explicit resolution is local and needs no store, child or network.
- The verified Git signal path is not an unverified child. It stays available.
- `--allow-network` means nothing to local inspection. The source resolver
  rejects it there as conflicting.

`receipt()` returns the effective mode for output. It names each disabled store
and its flag, and holds no path, credential or configuration value:

```json
{"response_cache":{"state":"disabled","by":"dry-run"},"ledger":{"state":"disabled","by":"dry-run"},
 "runtime_state":{"state":"disabled","by":"dry-run"},"network":{"state":"blocked","by":"dry-run"},
 "unverified_children":false,"cross_process_coordination":false,"stateless":true}
```

Production callers open the cache through `EffectGate::open_cache`. The
storage-level `open_cache` stays public for storage tests and future
administrative commands. The rank pipeline, hook, dry-run preview and doctor
consume the gate in their own tasks. No shipped command reads the gate yet.

## Verification

`tests/effect_gate.rs` covers all 128 flag combinations in both scopes. The
76 conflicting combinations are rejected, and each of the 104 valid gates
(52 per scope) is exercised at real boundaries:
- the derived restrictions, receipt and consent match an independent
  specification;
- the response cache is created on disk only when enabled. When disabled,
  nothing appears in the fixture tree and an unsafe location is never examined;
- a qualified-looking cass fixture starts only when unverified children are
  allowed. Otherwise it fails with `UnsupportedSourceMode`, and an explicit
  cass session maps to `unsupported-source-mode` (exit 7) without discovery;
- a loopback acceptor sees a TCP connection only for authorized consent. Blocked
  runs refuse before connecting: offline is `cache-miss` (exit 11), dry-run and
  unauthorized are `network-denied` (exit 8). The credential never appears in
  errors;
- trusted configuration is read and an explicit request resolves in every
  mode, with no state created.

Each prohibited effect has an observed positive twin. Snoozes are not
implemented yet, so snooze reads are not covered.
