# Changelog

## Unreleased

### Fixed

- Matrix source-reference validation rejects unittest methods overwritten or
  deleted in the class body instead of counting their earlier declarations as
  test evidence. A surviving method still makes the class reference eligible;
  declaration validation remains distinct from an execution receipt.

### Added
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
  and separate raw versus rounding-normalized probabilities. Synthetic codec
  checks do not establish recorded-live response compatibility or transport readiness.
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
