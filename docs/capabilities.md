# Capability registry (P4)

`sr capabilities [--json]` prints the registry of this build as JSON
(`sr.capabilities.v1`). `src/capabilities.rs` is its single source. It holds:

| Field | Contents |
| --- | --- |
| `commands` | Every command in the roadmap, marked `implemented` or `planned` with its earliest phase |
| `planned_flags` | Flags the parser knows but refuses until their phase ships; currently `doctor --descriptions` (P9) |
| `adapters` | Adapter support, identity and visibility semantics, tested and unverified versions, and conformance evidence |
| `schemas` | Output, configuration, roster import/listing/snapshot/diff, retrieval and question-policy versions |
| `features` | Compiled Cargo features and whether each is implemented. `tui` is reserved and not implemented |
| `limits` | Every default resource limit, with its name, unit and maximum |
| `exit_codes` | Success, plus every error kind with its exit code |

The command inventory, phases and adapter records come from
`adapter::foundation_capabilities`. That document stays the pinned P0 fixture,
so the registry cannot list a command or adapter the foundation does not
declare. Limits come from `limits::ALL_DEFAULT_LIMITS`, and exit codes from
`output::ErrorKind::ALL`.

The implemented subcommands are `rank`, `roster`, `doctor`, `capabilities`,
`demo`, `hook`, `install-hook`, `uninstall-hook`, `stats`, `observe`, `feedback`,
`snooze`, `budget`, `replay`, `eval`, and `ledger`, plus help and version.
`calibrate` (P8), `tui` and `gaps` (P9) remain planned. An empty `tui` Cargo
feature does not implement the command.

The registry states what this binary runs. It claims no phase acceptance,
provider availability, native harness support or installed-version
compatibility. The Claude adapter stays unverified with no tested version, so
no native advice is claimed.

Observation source boundaries are separate from phase acceptance. `observe`
refuses nonempty history whose active lineage cannot be resolved, before writing
loads or advancing a cursor. An explicit branch selects a normalized lineage;
for unlabelled native records it names the local ledger branch and does not
authorize an ambiguous fork. Empty and unambiguous unlabelled history remain
supported.

New explicit-producer imports use framed, versioned observation and cursor keys,
so producer/session IDs containing colons and distinct agent identities remain
separate. Imports cannot name a native cursor by repeating its key as a session
ID. Legacy rows are preserved; their missing producer provenance is not
reconstructed. New imported loads retain unknown exposure attribution, since raw
session/branch IDs alone cannot establish the source of a recorded exposure.
Producer-aware imported exposure attribution remains unqualified. Native
observation attribution and repeated-delivery behavior retain their existing
contracts. These source controls do not establish independent usefulness,
representative traffic, a native harness qualification or P5 acceptance.

Behavior that must match the registry:
- Every implemented command answers `--help`.
- A planned command is refused as `invalid-usage` (exit 2). It is never
  partially run.
- `sr --help` names every implemented command and no planned one.
- `rank --save-case FILE` is implemented as explicit private case capture.
  Its conflicts with `--dry-run` and `--no-persist` are reported before capture.
  New cases use `schemas.replay_case = 2`; old binaries reject them instead
  of ignoring frozen semantics. Cases include versioned frozen inputs: canonical logical Jev requests, validated
  full responses, option/content maps, local membership/eligibility, and the actual
  numeric scoring adjustments. `schemas.frozen_replay_inputs` and
  `schemas.replay_computation` identify those contracts. Capture is opt-in and
  private; replay never imports its answers into the live cache.
- Replay reports input/stage completeness and checks the recorded computation,
  target, dependency, build and numeric probe profile before exact recomputation.
  A different profile is partial/incompatible. Legacy cases can still perform
  limited local recomputation, but cannot establish exact-input compatibility or
  a passed parity gate. These checks do not establish usefulness or phase acceptance.
- Bare `sr` is `sr rank`. Flags given without a subcommand are rank flags.

## Verification

`tests/capabilities_contract.rs` runs the real binary and checks:
- the printed registry equals the library registry;
- every implemented command answers `--help`;
- every planned command is refused;
- help names only implemented commands, includes `--save-case`, and hides
  planned flags;
- each planned flag is refused without starting its planned behavior;
- bare `sr` reaches ranking;
- limits, error kinds and adapter support match their sources.
