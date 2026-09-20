# Third-Party Notices

This file records code copied or adapted into SkillRanker from other
repositories, with pinned source revisions, the applicable license text, and
the local adaptations. Imported code is maintained in this repository; there
is no runtime dependency on the source projects.

## meta_skill — secret scanner

- **Source repository:** <https://github.com/Dicklesworthstone/meta_skill>
- **Source file:** `src/security/secret_scanner.rs`
- **Inspected revision:** `c9a616bcb29c89e640a95f2bca344c3053fdf7d0`
- **Source file SHA-256:**
  `0fdb1cefbd8b6df7175b0a6970424d816205697244af2412c9ba7b11ab7ca6d4`
- **License:** MIT License (with OpenAI/Anthropic Rider), Copyright (c) 2026
  Jeffrey Emanuel. The full license text, including the rider, is preserved
  verbatim in [`LICENSE`](LICENSE); the source repository's `LICENSE` file is
  byte-identical to it. This is **not** unmodified MIT, and it is **not** the
  OCR model-weight notice.
- **Local copies and adaptations** (local file: `src/privacy/redaction.rs`):
  - Adapted the fixed secret-detection pattern set (AWS access/secret keys,
    GitHub tokens, JWTs, Bearer tokens, generic API-key/secret/password
    assignments, database URLs with credentials, Slack tokens, private-key
    headers) and the overlapping-range merge approach from
    `scan_secrets`/`redact_secrets`, plus the optional Shannon-entropy heuristic.
    Base64/hex classification and padding helpers were not imported.
  - Adapted regression ideas for overlapping matches producing a single
    redaction, preserving safe neighbors, and recognizing secret families.
  - **Removed** the `SecretMatch::masked_preview` accessor and stored match
    copies. Internal matches retain only byte ranges; diagnostics retain no
    secret text. JSON inspection temporarily decodes private input strings.
  - **Removed** application coupling: no `ms` crate, logging, or evidence
    plumbing; the module is a pure bounded-text transformer.
  - **Added** whole-payload (assembled serialized request) inspection with
    reject-not-rewrite semantics, complete-field-before-truncation ordering
    helpers, merged-range counting without match retention, and
    truncation-boundary tests (a secret split by a naive truncation must not
    survive; redaction always precedes truncation).
  - Upstream patterns matched against a bounded key prefix group in some
    assignment-style patterns; the local version redacts the full assignment
    value so no suffix of the secret remains.
- **Test provenance:** `tests/redaction_contract.rs` adapts the upstream
  overlap and secret-family regression ideas with synthetic values and adds
  full private-key blocks, entropy overlap extension, repeated-pass stability,
  Unicode/truncation boundaries, nested/escaped JSON inspection and size/depth
  rejection. These are library tests, not live-provider or hook evidence.
- **Consumers:** the privacy module (`src/privacy`) and, after later
  integration beads, outgoing provider payload preparation. Nothing in this
  repository sends data to any third-party service as a result of this copy.

## frankenscipy — statistical confidence intervals and logsumexp

- **Source repository:** <https://github.com/Dicklesworthstone/frankenscipy>
- **Source files:**
  - `crates/fsci-stats/src/lib.rs` (SHA-256: `01dd4dcc8fed57fcb1b6a6bd4e1e74ff84ad69624a4af9666220e4cdcf9f75a6`)
  - `crates/fsci-special/src/beta.rs` (SHA-256: `5a64958a2e4742352ebf565935c1a7e87a63ed90c1b33778bee51c220b617a55`)
- **Inspected revision:** `213a417c739025ed865a3875c89a7d86726ecb5a`
- **License:** MIT License (with OpenAI/Anthropic Rider), Copyright (c) 2026
  Jeffrey Emanuel. The full license text, including the rider, is preserved
  verbatim in [`LICENSE`](LICENSE).
- **Local copies and adaptations** (local file: `src/evaluation/numerics.rs`):
  - Adapted `wilson_ci` for two-sided binomial proportion intervals.
  - Adapted `clopper_pearson_ci` and one-sided `clopper_pearson_one_sided_upper`
    with exact `BetaDist` PPF integration and stable closed-form zero-event
    calculation (`-expm1(log(1 - alpha) / n)`).
  - Adapted Lentz's continued fraction method for incomplete beta (`betacf`)
    and bracketed root-finding for `betaincinv`.
  - Adapted Wichura (1988) AS241 rational approximation for standard normal
    quantile (`standard_normal_ppf`).
  - Adapted max-shifted `logsumexp` for finite and infinite floating point slices.
  - **Removed** workspace and transitive dependencies: zero external crate dependencies,
    safe standalone pure Rust.
- **Consumers:** the evaluation module (`src/evaluation/numerics.rs`).

## franken_numpy — deterministic PCG64-DXSM sampling

- **Source repository:** <https://github.com/Dicklesworthstone/franken_numpy>
- **Source file:** `crates/fnp-random/src/lib.rs`
- **Inspected revision:** `52700bc2a6d2ab48dab5e608b1cfa34d781a7bb1`
- **Source file SHA-256:**
  `9c92a7eedc12518d10ccda345239929bc7c83e8eb060321afc447260f753c472`
- **License:** MIT License (with OpenAI/Anthropic Rider), Copyright (c) 2026
  Jeffrey Emanuel. The full license text, including the rider, is preserved
  verbatim in [`LICENSE`](LICENSE).
- **Local copies and adaptations** (local file: `src/evaluation/sampling.rs`):
  - Adapted `Pcg64Dxsm` 128-bit state generator with DXSM output permutation.
  - Uses rejection followed by modulo reduction for unbiased bounded integers
    (`bounded_u64`), rather than NumPy's multiply-high bounded draws.
  - Uses a custom 64-bit seed mapping, not NumPy's SeedSequence initialization;
    high-level seeded samples do not claim NumPy bit parity.
  - Adapted Floyd's subset selection followed by Fisher-Yates shuffling for
    ordered draws without replacement (`choice_indices(pop_size, size, false)`).
    `SAMPLING_VERSION` identifies this ordered stream as
    `sr-evaluation-sampling-v2`; historical unshuffled draws differ.
  - Adapted Fisher-Yates shuffle for `shuffle_slice`.
  - **Removed** ndarray, rayon, and getrandom dependencies: pure deterministic safe Rust.
- **Consumers:** the evaluation module (`src/evaluation/sampling.rs`).
