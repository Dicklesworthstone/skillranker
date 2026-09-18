# Cache Namespace, Request, and Decision Fingerprints

Implementation documentation for boundary `sr-roadmap-l1i.5.7`.

## Architectural Overview

SkillRanker uses a two-tier fingerprinting model backed by keyed BLAKE3 hashing to ensure exact cache lookup, session isolation, and duplicate delivery detection without exposing prompt text or permitting offline dictionary attacks on low-entropy prompts.

```text
Session + Workspace + Branch + Context Epoch + Key Generation
  -> CacheNamespace
       +
Request Inputs (Stage, Redacted State, Candidates, Questions, Model, Versions)
  -> RequestFingerprint (Keyed BLAKE3)
       +
Decision Inputs (Loaded References, Exclusions, Policy, Prior Snapshot, Snoozes)
  -> DecisionFingerprint (Keyed BLAKE3)
```

## Security & Privacy Properties

1. **Local Secret Key (`CacheKey`)**:
   - Stored hashes are computed using a 32-byte secret key generated from the operating system CSPRNG (`/dev/urandom`).
   - Prevents dictionary attacks where an attacker precomputes BLAKE3 hashes of known prompts to verify whether a user ran a particular prompt.
   - Debug and display implementations strictly redact the secret key (`CacheKey(<secret-key>)`), ensuring raw key bytes never appear in logs or diagnostics.
2. **Namespace Isolation (`CacheNamespace`)**:
   - Cache entries are scoped to `(workspace_id, session_id, branch_id, context_epoch, adapter_id, adapter_version, harness_id, key_generation)`.
   - Different sessions never share a cache result, even if identical prompt text or tool invocations occur.
   - Advancing `key_generation` invalidates the entire namespace without requiring physical file deletion.
3. **Stage & Shortlist Sensitivity**:
   - Wide and Rerank stages produce distinct fingerprints.
   - Candidates are bound by skill ID, exact content hash, and excerpt hash; candidate ordering is preserved.
4. **Finite Floating-Point Canonicalization**:
   - `RankingPolicySnapshot` validates that all weights and thresholds (`w_fit`, `w_prior`, `w_phase`, `gate_threshold`, `fit_threshold`) are strictly finite numbers.
   - Any `NaN` or `Inf` value immediately returns `FingerprintError::NonFiniteWeight`.
   - Floats are hashed via their little-endian bit representations (`to_bits().to_le_bytes()`).

## Duplicate Hook Delivery (`EventDeliveryKey`)

Duplicate hook delivery is detected independently of cache lookup:
- Binds `(session_id, branch_id, event_type, transcript_generation, cursor_offset, prompt_fingerprint)`.
- If an explicit `delivery_id` is supplied by the harness, it is incorporated and marked unambiguous (`is_ambiguous = false`).
- If no `delivery_id` is provided and both `transcript_generation == 0` and `cursor_offset == 0`, the delivery is flagged as `is_ambiguous = true`.
- Ambiguous deliveries are excluded from exposure-based training to avoid false negative or duplicate positive observations.

## Verification

The contract is verified in `tests/cache_fingerprint.rs` (7 tests, 0 failures):
- `test key_protection_and_generation`: verifies CSPRNG generation and zero raw key leakage in debug representations.
- `test request_fingerprint_determinism_and_canonical_equivalence`: verifies canonical stability.
- `test request_fingerprint_stage_and_field_sensitivity`: tests 10 individual field changes, ensuring every single field alters the fingerprint.
- `test namespace_isolation_across_sessions_and_key_generations`: confirms different sessions and key generations produce distinct fingerprints.
- `test decision_fingerprint_sensitivity`: confirms loaded references, exclusions, policy parameters, snapshots, and snoozes alter the decision fingerprint.
- `test non_finite_policy_weights_rejected`: verifies fail-closed rejection of `NaN` and `Inf`.
- `test event_delivery_duplicate_detection`: verifies duplicate key hashing and ambiguity flags.
