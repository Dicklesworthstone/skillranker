# Doctor readiness and policy fingerprint (P4)

`sr doctor [--json | --table] [--offline | --allow-network]` reports each
prerequisite as its own state. A failed check names one concrete next step,
and the report lists the local commands that still work. Doctor is local: it
sends no request, installs nothing, migrates nothing, writes no state and
changes no configuration. Invalid configuration fails first, with
`invalid-configuration` (exit 2), no fingerprint and no roster discovery.

| Check | States | Next step when failed |
| --- | --- | --- |
| `configuration` | `valid`, with `policy_fingerprint` | invalid configuration exits 2 instead |
| `input` | `not-evaluated`: doctor reads no session | — |
| `roster` | `ready`, `empty`, `no-advisory-candidates`, `unusable`, `timeout` | add skills, or inspect `sr roster --json` |
| `credential` | `present` or `absent`; `verified` is always `false` | create your own key in the TypeSafe console |
| `network` | `authorized` (with `source`), `not-authorized`, `blocked` (with `by`) | `--allow-network`, or trusted `network.enabled = true` |
| `transport` | `untested`, `previously-verified`, `invalidated` | — |
| `ledger` | `not-available`, `blocks_ranking: false` | — |
| `hook` | `mode` and `mode_sources`; `installation: not-checked`; `snoozes: not-available` | — |

Notes:
- A present key is presence, never authentication. Only a separately
  authorized, budgeted live check could verify transport.
- Roster readiness uses the documented Claude roots through the same
  resolution as `sr roster`. It reports counts, `partial`, discovery
  `source_causes` and excluded-record `record_causes`. Claude visibility is
  unverified in this build, so skills appear but none is offered as automatic
  advice. Doctor reports `no-advisory-candidates` rather than `ready`.
- Network consent comes from `effects::EffectGate` for the given flags and
  trusted configuration.

## Transport evidence

`readiness::TransportEvidence` records a live check's time, scope and
`TransportIdentity`: the `sr` version, runtime and TLS trust configuration,
canonical endpoint origin, model and timeout. `assess_transport` returns:
- `previously-verified` only when every identity field matches. This is
  historical and never current health.
- `invalidated`, naming each changed field, when any field differs. The same
  applies to a record dated more than five minutes in the future.
- `untested` without a record.

No live check writes transport evidence in this build, so doctor always
reports `untested`.

## Policy fingerprint

`EffectiveConfig::policy_fingerprint()` digests every effective value under the
configuration schema version. Values identify the policy, not their sources:
the same value from another layer gives the same fingerprint. Set-valued lists
(exclusions, roots) are sorted first. Credentials never enter it. It appears in
`sr doctor --config` and in the readiness `configuration` check. Only a valid
configuration has one.

## Verification

`tests/doctor_readiness.rs` runs the real binary with a cleared environment and
isolated `HOME`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_CACHE_HOME` and
`XDG_STATE_HOME`. It covers:
- an empty home: missing roster, key and consent each get a step, the ledger
  blocks nothing, and no state directory appears;
- a present key that is not called verified, with transport still untested;
- network consent from the flag, trusted settings, `--offline` and the
  conflicting pair;
- roster counts, unverified visibility, an unreadable root and a malformed
  skill;
- invalid project configuration: exit 2, no fingerprint, no report;
- fingerprint stability across layers and set order, changes when a value
  changes, and independence from the credential;
- the table listing every check;
- transport evidence invalidated by each identity field and by a future
  timestamp, with unknown record fields refused.

Eight planted mutations were each caught.
