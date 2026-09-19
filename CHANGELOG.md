# Changelog

## Unreleased

### Fixed

- `sr rank` retries transient provider failures (429, 5xx, connection errors)
  within one per-invocation allowance of two logical requests and four HTTP
  attempts. It honors `Retry-After` and the entry deadline, and never retries
  authentication or invalid answers. Trusted policy is re-authorized before
  every attempt, retries included. Each rank had made exactly one attempt per
  stage with no allowance.
- A rank failure after its input is admitted is a full `unavailable` decision.
  This covers a superseded policy, withdrawn consent, a roster change, a
  timeout and a provider failure. The decision keeps the stages that ran and
  every request, attempt and token already incurred. An attempt that started
  but returned no usage is counted as unknown usage, not zero.
- `sr rank --offline` with no cached result reports `cache-miss` (exit 11), as
  documented, instead of `network-denied`. Provider admission uses the shared
  admission check for both stages.
- `sr rank` discovers and revalidates Claude user skills in
  `$HOME/.claude/skills`, as `sr roster` does. It had looked under the
  configuration root, so personal skills were invisible to ranking.
- Rank decisions report what actually ran. Abstentions after a wide or rerank
  call carry their real usage, stage estimates and returned models instead of
  zeros and nulls. Ranked output reports real candidate counts, digests of the
  evaluated candidate sets rather than fixed placeholders, and the provider's
  returned model next to the requested alias.
- Withdrawing trusted network consent before a pending send refuses it as
  `network-denied` (exit 8), as documented, rather than `superseded`.
- `sr rank` reads `--context`, stdin and `--roster` as bounded regular files:
  an oversized context is `oversized-input`, and a FIFO, device or symlink
  leaving its directory is refused with a typed error instead of blocking or
  being read. The trusted `context.profile` and configured ranking weights now
  apply (both were hard-coded), every effect restriction comes from the effect
  gate, and the wide request fingerprint binds the exact request bytes and
  effective endpoint under a fresh random key rather than a fixed one.
- Scoped roster authority withholding to affected invocation names and continue
  past per-file read errors during Claude plan resolution (`sr-fzia`). Individual
  malformed or escaping skill files now withhold authority only for the callable
  names they could claim, leaving the remaining valid skills advisory and
  inspectable, while unknowable layouts and root-level failures remain globally
  withheld.
- Several test fixtures keep their temporary trees after the run. They named
  them only by process ID and a counter, so a reused process ID on a build
  worker could pick up an earlier run's tree. The names now include a
  timestamp, and the trees are created exclusively.
- Matrix source-reference validation rejects unittest methods overwritten or
  deleted in the class body instead of counting their earlier declarations as
  test evidence. A surviving method still makes the class reference eligible;
  declaration validation remains distinct from an execution receipt.

### Added
- `sr rank` keeps an exact response cache in the owner-only platform cache
  directory. An identical request in the same session is answered without
  contacting Jev, with zero new usage, for up to ten minutes; `--offline` can
  serve a complete cached pair. Fingerprints use a random key kept in the
  store. The cache never crosses sessions, and a cached wide answer is never
  paired with a fresh rerank. `--no-cache`, `--no-persist` and `--dry-run`
  never touch it. The cache store schema is now version 2; a version 1 store
  is refused and ranking continues uncached.
- `sr capabilities --json` prints this build's capability registry
  (`sr.capabilities.v1`): every command as implemented or planned, flags
  refused until their phase, adapter evidence, schema versions, features,
  limits and exit codes. The foundation document and `limits`/`ErrorKind`
  supply it. Planned commands are refused, `rank --save-case` is refused until
  P5, and bare `sr` now ranks as documented.
- A catalog-driven product e2e runner, `scripts/e2e/product.sh`, runs any
  suite in `scripts/e2e/product/`. A complete run may leave ignored only opt-in
  tests the catalog declares with a reason. A test's own `--exact` child run
  neither counts as a target nor rescues a failure. The roster suite now uses
  the same runner.
- Product e2e case evidence. `scripts/e2e/product/roster.json` maps each P2
  e2e case to the real Rust tests that establish it. `scripts/e2e/product_cases.py`
  checks the mapping against the sources and the contract matrix, and evaluates
  each case from the actual test log. The roster suite now runs Quill-query,
  local-inspection and P2 gate targets too, and it passes only if every case
  passes. `tests/p2_gate.rs` checks that every admitted option maps to a
  currently authorized file whose bytes match the record. An import test shows
  that importing a roster changes nothing on disk. Stale P2 matrix test
  references now name the tests that exist.
- `sr doctor` readiness report: configuration validity with a policy
  fingerprint, roster counts and causes, key presence (never "verified"),
  network consent, transport evidence state, ledger and hook mode. Each is
  reported separately, with one next step per failed check and no request,
  installation, migration or state write. `sr doctor --config` now includes the
  policy fingerprint. Only a valid configuration has one. Historical transport
  evidence is invalidated by any change in version, runtime, endpoint, model or
  timeout. No live check writes it yet.
- Central `effects::EffectGate`. One validated set of effect flags now derives
  the context source policy, response-cache, ledger and runtime-state access,
  history availability, network consent and a printable effective-mode
  receipt. A disabled ledger reports history as withheld (unknown), not empty.
  Tests observe cache files, cass child starts and loopback provider sockets
  for every valid flag combination. No shipped command consumes the gate yet.
- Shared bounded configuration loading and effective-policy file refresh through
  `cli::ConfigFiles`, retaining invocation CLI/environment overrides and typed
  receipt comparisons. CLI precedence and real-file refresh cases are exercised;
  rank admission/publication integration remains pending.
- Optional local `context::signals` collection: bounded allowlisted filenames
  and trusted executable presence, safe Git 2.36+ status with fsmonitor disabled,
  private relative paths, omission counts, and a 250 ms stage budget. No tool
  probing, network calls, or provider/CLI integration is added.
- Pure `context::parse_normalized_context` decoding with a 1 MiB input bound,
  depth-64 JSON validation, duplicate-key and definition rejection, and fixed
  diagnostics. Parsed identities and paths remain local declarations, not
  filesystem, network, or native-observation authority.
- Owned bounded subprocess execution under `subprocess`: trusted-resolved
  argv executables with canonical-path authority grants, empty-by-default
  environments that reject provider/proxy/loader variables, independently
  bounded pipes, deadline cancellation with Unix process-group kill and
  reaping, and no shell interpolation or detached workers. Requires `nix`
  signal/process support on Linux/macOS; other platforms refuse before spawn.
- Pure TypeSafe Jev Choice/Noul codecs with bounded JSON, duplicate-key
  rejection, exact question/option matching, validated usage and distributions,
  and separate raw versus rounding-normalized probabilities. One consented
  synthetic-content live capture (model jev-1.13.0 at capture time) is recorded
  with provenance; it is a time-bounded wire observation, not a model pin, and
  establishes no transport readiness or ranking gate.
- Standalone bounded secret redaction under `privacy::redaction`, with
  whole-field-before-excerpt scanning, merged-match counts and final JSON
  payload inspection. Pattern/test provenance is recorded in
  [third-party notices](THIRD_PARTY_NOTICES.md). This library boundary does
  not yet establish live ranking or hook integration.

- Rust 2024 package with a pinned toolchain/lockfile, help/version bootstrap CLI,
  identity/provenance types, bounded resource accounting, and frozen evaluation
  fixtures. [Initial verification](docs/verification-p0.md) and the
  [foundation audit](docs/verification-foundation-audit.md) identify the exact
  source and focused checks; they do not certify product ranking.
- Versioned [adapter/capability contracts](docs/adapter-contract.md) and
  [decision, error, trace and report contracts](docs/output-contract.md), with
  synthetic examples kept separate from real-harness or live-provider evidence.
- Pure [configuration and authority contracts](docs/config-contract.md) for
  trusted layers, disclosure restrictions, consent, persistence modes and
  effective-policy revalidation. Filesystem loading and runtime effects retain
  their separate integration gates.
- Bounded offline test runner, sanitized evidence records and phase-specific
  contract coverage scaffolding. These validate the test infrastructure;
  future product cases remain planned until their own execution evidence exists.
- Comprehensive SkillRanker design: live-session context, two-pass Jev ranking,
  local calibration, hook integration, and an inline terminal interface.
- Product and CLI documentation, repository engineering rules, and development
  file conventions.
- MIT License with the OpenAI/Anthropic Rider.
