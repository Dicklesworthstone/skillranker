# P0 contract acceptance

The P0 foundation contracts are accepted for implementation commit `cb3cafe`.
The combined verification below covers the repaired matrix, adapter,
configuration diagnostics, and live-batch duration boundaries.

Tracking gate: `sr-roadmap-l1i.1.11`. This review evaluates the foundation
contracts required before P1 transport, P2 roster/privacy, and P3 context work.
Closing this gate admits those implementation tracks; their own acceptance
gates still apply.
It does not certify a ranking command, live Jev request, native harness,
storage implementation, product latency, quality promotion, or release target.
The bootstrap binary still exposes only help and version; the capability
registry keeps the 19 later commands explicitly planned.

## Contract agreement

The identity, normalized-context, resource, output, configuration, and adapter
contracts were tested together, including the final configuration error-kind
mapping and complete command phase registry. Jev remains the sole ranking
provider. Possession of a TypeSafe key does not grant network consent. Provider
answers, project configuration, imported context, and synthetic demonstrations
cannot acquire execution, persistence, or native-session authority.

The dependency check resolves one Asupersync source and one Quill source across
normal/build/dev edges with all features. Asupersync is pinned to
`81fb7b579ce5f161622f1524391f5202a641cc2e`; FrankenSearch is pinned to
`39047c44c3a92ceb71d25c602913b8b2888e2fe7`. The selected toolchain is
`nightly-2026-08-31`. No prohibited runtime/search package appears in that graph.
The existing source qualification and actual notices remain in
[dependencies.md](dependencies.md),
[dependency-qualification.json](dependency-qualification.json). No upstream
source slices have been copied into this package; actual imports must carry
their required notices. The project license is unchanged.

## Coverage and evidence boundaries

[The matrix](../tests/contract_matrix.toml) has 77 boundaries and explicitly
accounts for all 179 non-epic roadmap members in this reviewed snapshot.
[The authority inventory](../tests/contract_authority.toml) binds each boundary's
owner, title, phase, member list, platforms, features, suite, cases, and assertions.
Future aggregates are planned coverage obligations; they must be expanded into
individual executable boundaries before execution can be claimed. New roadmap
members require an explicit inventory update; they cannot silently disappear.

Seven false accepts in the previous validator were reproduced before repair:
missing authority, a wrong but existing owner, unknown suite, unknown case,
unknown assertion, empty selection, and an omitted required boundary. Earlier
`.1.12` closure established runner mechanics but did not resolve these matrix
requirements. This gate includes their repair and regression evidence.

Pure Rust and check-script contract rows declare their separate e2e suite
`not-applicable` with empty e2e selections. Their evidence comes from actual
Rust tests or check-script execution, not borrowed Python fixture scenarios.
The bootstrap Rust suite itself launches the compiled `sr` process. Actual
runner infrastructure rows retain their concrete smoke/certification cases.

Default matrix validation checks declarations and source references. It makes
no unit execution or product acceptance claim. Source resolution recognizes a
conservative declaration subset; it does not execute Python or replace Cargo
discovery, compilation, and test results. The acceptance review separately
matches every required Rust reference against an executed passing test and
rejects skipped/filtered unit evidence. `--require-mechanics` additionally
requires complete smoke and outer certification reports matching the independently
computed source, binary, lockfile, toolchain, platform, and feature identity.
The outer certificate has 54 checks, not merely the 19 child scenarios. Its
`satisfied` assertions are distinct from the child `behavior` assertions.
Missing, partial, stale, incompatible, or fixture-interpreter evidence presented
as Rust/product proof is refused. Artifact integrity is not cryptographic
attestation against a malicious artifact author.

Rust contract tests are established by their own required-remote Cargo execution
and source-content receipts. Python fixture interpreter receipts cannot replace
them. P0 matrix status fields are declarations captured before phase acceptance;
the combined run record below establishes which current inputs actually ran.
The acceptance decision resides in this review and the Beads gate, rather than
in a self-updating matrix status field that would invalidate its own source hash.

## Combined verification

The accepted source is implementation commit `cb3cafe`, including the matrix,
adapter, configuration, and duration repairs below. All 56 build, runtime, test,
and embedded-document inputs match both frozen verification checkouts exactly.
The commands ran on base `92553e3` plus the two configuration-file overlays later
committed in `cb3cafe`; the machine-readable record distinguishes that executed
base and overlay from the equivalent published implementation.

Python/mechanics artifacts are retained at
`/data/tmp/skillranker-p0-accepted-w7u373pr`. Remote Rust commands, outputs, worker
hashes, and failed attempts are retained at
`/data/tmp/skillranker-adapter-review-orgyof57`. See
[the machine-readable record](verification-p0-acceptance.json) for hashes and
exact commands. It records the current combined results separately from the
retained historical attempts.

The remote Rust runs use an isolated Git base plus two explicit overlay paths.
RCH reports their common overlay fingerprint. Independent samples of all 77
selected source files match the frozen manifest for all four accepted runs. These
samples were taken during the builds; they are not post-command source-content
receipts. Earlier attempts with post-command receipts failed at the SSH barrier
and are excluded from successful verification. No local fallback is accepted as
remote proof.

| Check | Result |
| --- | --- |
| Rust contract and real bootstrap CLI tests | 97 passed; zero failed, ignored, or filtered integration tests |
| Matrix declaration and real receipt regressions | 20 passed |
| Runner engine and fresh-eyes boundary regressions | 25 passed |
| Process cleanup regressions | 2 passed |
| Independent certificate regressions | 15 passed |
| Evaluation policy regressions | 43 passed |
| Synthetic evaluation fixtures | 12 cases validated; no benchmark claim |
| Complete smoke and independent outer certificate | 3/3 and 54/54, then revalidated through the matrix gate |
| Documentation, dependency graph, formatting, and Python lint | Passed |
| Remote default all-target check | Passed |
| Remote default all-target Clippy | Passed |
| Remote all-feature all-target Clippy | Passed |

Every required Rust matrix reference has an actual passing test line and an
invoked integration binary. The complete suites contain no skipped tests. The
proof is Linux x86-64 only. `tui` is an empty reserved feature boundary; an
all-feature build does not establish a TUI.

The current matrix UBS scan reports zero critical findings and 31 reviewed
warnings concerning strict scalar typing, caller-handled JSON errors, and file
descriptor ownership. This is a scoped scan, not a clean whole-repository claim.
Earlier source snapshots and their successful or failed commands remain in the
artifact directories and the versioned verification record. Their results do
not certify later source changes. No consequential assertion was weakened to
obtain the passing runs.

## Repairs included in acceptance

The subsequent matrix review tightened source reference resolution: ignored or
conditional Rust tests, Cargo-undiscovered nested files, Python helpers outside
`unittest.TestCase`, and overwritten test methods cannot stand in for test
evidence. Pure contract rows now explicitly mark a separate e2e suite as
not applicable; they still require their own executed contract checks.

The adapter repair rejects malformed optional hook fields and contradictory
version declarations, removes private extension keys and commit text from Debug
output, and rechecks native-advice support before transferring qualification.
Cass session provenance cannot grant native hook advice. Version qualification
requires a usable commit identity or a binary digest. These changes preserve
valid additive fields and properly qualified support records.

The configuration repair discards unknown key text from diagnostic state while
retaining the layer and error category. Unknown keys can themselves contain
private data; bounding their length or restricting them to ASCII is insufficient.
The batch-limit repair rejects zero runtime even when duration arithmetic,
rather than the positive-duration constructor, produced that value. Both repairs
retain valid-input counterparts in their regression tests.

## Unresolved facts and their owners

| Fact still requiring evidence | Owning bead(s) | Gate before claiming support |
| --- | --- | --- |
| Actual TypeSafe request limits, model aliases, answer/usage semantics | `sr-roadmap-l1i.2.5`, `.2.6`, `.2.10` | Typed fixtures and separately consented, bounded live Jev spike |
| Public trust roots, DNS/TLS cancellation, bounded retries and total deadline | `sr-roadmap-l1i.2.1`, `.2.2`, `.2.7`, `.2.9`, `.2.11`, `.2.12` | Real transport and stalled-operation proof on the selected build |
| Harness-visible skill precedence, collisions and snapshot revalidation | `sr-roadmap-l1i.3.4`, `.3.5`, `.3.11`, `.3.16`, `.3.17` | Actual bounded filesystem and visibility fixtures |
| Quill candidate coverage and deterministic overflow behavior | `sr-roadmap-l1i.3.8`, `.3.9`, `.3.10`, `.3.16` | Sole-engine integration, feature denylist, and retrieval evidence |
| Claude prompt timing, branch identity, cass producer coverage and disclosure | `sr-roadmap-l1i.4.3`, `.4.4`, `.4.5`, `.4.6`, `.4.13`, `.4.14` | Versioned adapter conformance and exact-source tests |
| Writable SQLite version, cache isolation and concurrent persistence | `sr-roadmap-l1i.5.6`, `.5.8`, `.5.10`, `.5.20`, `.6.26` | Real engine identity, transaction, crash, and contention evidence |
| Installed Claude behavior, admission accounting and full hook latency | `sr-roadmap-l1i.7.10`, `.7.11`, `.7.12`, `.7.13` | Actual supported harness and measured full-invocation distributions |
| Usefulness, interruption risk, held-out relevance and rollout | `sr-roadmap-l1i.8.1`, `.8.3`, `.8.4`, `.8.5`, `.8.8` | Independent labeled cohorts and prespecified promotion gates |
| Learned-policy benefit and valid sequential monitoring | `sr-roadmap-l1i.9.8`, `.9.9` | Held-out improvement and explicit evidence/alpha accounting |
| Native Codex, omp/pi, and Grok support | `sr-roadmap-l1i.10.15`, `.10.17`, `.10.19`, `.10.27`, `.10.28`, `.10.29` | Separate qualification and dispositions; deferred implementations remain unavailable |
| Non-Linux targets, packaging and release artifacts | `sr-roadmap-l1i.8.7` | DSR-built and independently verified target artifacts |

These are assigned implementation or qualification obligations. None can be
inferred from successful P0 infrastructure checks or finished-product README prose.
