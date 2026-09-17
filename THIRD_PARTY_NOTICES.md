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
