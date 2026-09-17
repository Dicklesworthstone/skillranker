# P0 contract acceptance

Acceptance remains open while the reference/evidence-role repairs and Rust
adapter strictness/privacy corrections receive current-source verification.
The successful snapshots below do not certify the newer adapter regressions.

Tracking gate: `sr-roadmap-l1i.1.11`. This review evaluates the foundation
contracts required before P1 transport, P2 roster/privacy, and P3 context work.
Those tracks remain blocked until the owned Beads gate closes.
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
accounts for all 179 non-epic roadmap members in this accepted snapshot.
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
no unit execution or product acceptance claim. `--require-mechanics` additionally
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

See [the machine-readable record](verification-p0-acceptance.json) for exact
commands, input hashes, worker receipts, logs, and retained failure attempts.
The first frozen checkout is `/data/projects/skillranker-p0-proof-d372lxco`, based on
`82a508236b51441bb5ddd13ee83821b017a918b0` plus the four reviewed matrix files.
Its 54 build/test source inputs match the first combined implementation at
`0daebc1`. Subsequent changes affect only the two matrix and two evaluation
Python scripts. A final frozen Python/mechanics run covers those changes; the
final checkout is `/data/projects/skillranker-p0-final-rrg_f1ya` at
`3391b6b27ff956cc508f5862c15d9962003f5c6a`. The original Rust receipts still
match every Rust/build input and every embedded
fixture, including README and the comprehensive plan. Documentation
and operational Beads exports have separately recorded hashes; they are outside
the runner source digest. The proof is Linux x86-64 only. `tui` is an empty
reserved feature boundary, and an all-feature build does not establish a TUI.

| Check | Result |
| --- | --- |
| Rust contract and real bootstrap CLI tests | 88 passed; zero failed, ignored, or filtered integration tests |
| Matrix declaration and real receipt regressions | 17 passed |
| Runner engine and fresh-eyes boundary regressions | 25 passed |
| Process cleanup regressions | 2 passed |
| Independent certificate regressions | 15 passed |
| Evaluation policy regressions | 43 passed |
| Synthetic evaluation fixtures | 12 cases validated; no benchmark claim |
| Complete smoke and independent outer certificate | 3/3 and 54/54, then revalidated through the matrix gate |
| Documentation, dependency graph, formatting, and Python lint | Passed |
| Remote default check and default/all-feature Clippy | Passed, with exact source-content receipts |

The independent commit review also repeated all 16 matrix tests on a frozen
snapshot. Its staged UBS scan had zero critical findings and 31 reviewed warnings
about strict scalar typing, exception handling across functions, and descriptor
ownership. This is separate from earlier runner scans with documented heuristic
critical findings; no clean whole-repository scanner claim is made.

The first combined remote check failed during SSH preflight and did not compile
locally. The original evaluation-test invocation used Python `-I`, which prevented
its local sibling import; the ordinary script invocation with user-site and
`PYTHONPATH` disabled passed. Both failed attempts remain in the artifact record.
No source assertion was weakened to obtain the passing runs.

A subsequent review (`sr-eval-audit-dwew`) reproduced invalid policy changes
that the evaluation checker had accepted, plus private unknown arguments in
usage errors. The policy repair enforces the frozen tie-break, baseline set,
cohort separation, interval methods, and loaded-reference/workflow preconditions.
The same argument-echo defect was independently reproduced and fixed in the
matrix CLI. Both changes have real refusal tests with successful counterparts;
the final refresh passed all 102 Python tests and complete matching mechanics reports.

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
