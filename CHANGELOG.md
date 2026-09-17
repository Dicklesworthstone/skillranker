# Changelog

## Unreleased

### Fixed

- Matrix source-reference validation rejects unittest methods overwritten or
  deleted in the class body instead of counting their earlier declarations as
  test evidence. A surviving method still makes the class reference eligible;
  declaration validation remains distinct from an execution receipt.

### Added

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
