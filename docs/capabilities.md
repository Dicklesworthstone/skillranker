# Capability registry (P4)

`sr capabilities [--json]` prints the registry of this build as JSON
(`sr.capabilities.v1`). `src/capabilities.rs` is its single source. It holds:

| Field | Contents |
| --- | --- |
| `commands` | Every command in the roadmap, marked `implemented` or `planned` with its earliest phase |
| `planned_flags` | Flags the parser knows but refuses until their phase ships, such as `rank --save-case` (P5) |
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

The registry states what this binary runs. It claims no phase acceptance,
provider availability, native harness support or installed-version
compatibility. The Claude adapter stays unverified with no tested version, so
no native advice is claimed.

Behavior that must match the registry:
- Every implemented command answers `--help`.
- A planned command is refused as `invalid-usage` (exit 2). It is never
  partially run.
- `sr --help` names every implemented command and no planned one.
- `rank --save-case FILE` is refused with `invalid-usage` and writes nothing.
  Its conflicts with `--dry-run` and `--no-persist` are still reported.
- Bare `sr` is `sr rank`. Flags given without a subcommand are rank flags.

## Verification

`tests/capabilities_contract.rs` runs the real binary and checks:
- the printed registry equals the library registry;
- every implemented command answers `--help`;
- every planned command is refused;
- help names only implemented commands and hides `--save-case`;
- the planned flag is refused without writing a file;
- bare `sr` reaches ranking;
- limits, error kinds and adapter support match their sources.
