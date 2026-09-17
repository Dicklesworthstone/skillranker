# Jev Distribution and Confidence Validation Contract

Contract specification and verification rules for TypeSafe Jev response decoding,
distribution validation, argmax checking, and usage accounting.

Satisfies boundary `p1_distribution_validation` (`sr-roadmap-l1i.2.6`).

## Invariants and Semantics

### 1. Finite Probabilities and Nouls
- Every probability in a Choice distribution and every Noul value must be a finite IEEE-754 float in the closed unit interval `[0.0, 1.0]`.
- Non-finite values (`NaN`, `+Infinity`, `-Infinity`) and out-of-range floats (`< 0.0` or `> 1.0`) are strictly rejected with `CodecError::InvalidProbability`.
- Missing probability values or question answers are treated as fatal errors, not imputed as zero.

### 2. Choice Distributions and Sum Tolerance
- A Choice distribution must contain exactly the options requested in the question criteria (no missing options, no foreign option IDs).
- Sum of distribution probabilities must be strictly positive (`sum > 0.0`) and within a tolerance of `1.0`:
  $$| \sum p_i - 1.0 | \le 10^{-4} \quad (\text{SUM\_TOLERANCE} = 10^{-4})$$
- Raw probabilities and the raw sum are preserved in `ChoiceAnswer`.
- Normalized probabilities are calculated on-demand by dividing raw probabilities by `raw_sum`, isolating rounding drift without modifying recorded provider evidence.
- Sum deviations exceeding `1e-4` produce `CodecError::InvalidDistribution`.

### 3. Argmax Validation and Deterministic Tie-Breaking
- The option returned in the `choice` field MUST be an argmax of the distribution:
  $$p(\text{choice}) = \max_{o} p(o)$$
- Exact ties are permitted: if multiple options share the maximum probability, the provider may return any member of the argmax set. Returning any option with probability strictly less than the maximum produces `CodecError::InvalidChoice`.
- Local deterministic tie-breaking by stable skill ID is provided via `ChoiceAnswer::deterministic_choice()`, which selects the lexicographical minimum option among argmax candidates.

### 4. Bounded Usage Integers and Unknown Usage Preservation
- `usage` contains `input_tokens` and `output_tokens`, strictly validated as non-negative 64-bit integers (`u64`). Floats, negative integers, strings, or nulls produce `CodecError::InvalidAnswer`.
- Unknown usage is never counted as zero:
  - Successfully concluded attempts accumulate known token counts in `CostReceipt`.
  - In-flight attempts that fail terminally (timeout, network disconnect, or malformed provider response) after bytes are sent across the wire increment `unknown_usage_attempts` and set `has_unknown_usage = true`.
  - Output serialization explicitly preserves `unknown_usage_attempts`.

### 5. Response Cap, Nesting Depth, and Sanitized Errors
- Response payloads must not exceed `MAX_RESPONSE_BYTES` (2 MiB), evaluated on decoded/decompressed bytes. Responses exceeding the cap produce `CodecError::TooLarge`.
- JSON nesting depth is enforced (max depth 64); deeper structures produce `CodecError::InvalidJson`.
- Duplicate JSON keys within any object are strictly rejected.
- Diagnostics and error display never echo raw response bodies, error messages, or untrusted option names. All provider contract violations map to `ErrorKind::InvalidProviderResponse` with exit code `CliExit::ProviderContract` (10).

## Verification Matrix

| Assertion / Case | Requirement | Verification Location |
|---|---|---|
| `tests/jev_contract.rs::distribution_sum_and_confidence` | Unit/property tests for sum, drift, and confidence | `tests/jev_contract.rs` |
| `exact_argmax` | Choice in argmax set, ties allowed, deterministic tie-breaking | `tests/jev_contract.rs` |
| `unknown_usage_preserved` | Integer validation, known accumulation, and terminal error preservation | `tests/jev_contract.rs` |
| `nonfinite-rejected` | Rejection of NaN, Inf, missing fields, type mismatches | `tests/jev_contract.rs` |
| `distribution-sum-tolerance` | Boundary tests around 1e-4 tolerance across 255 options | `tests/jev_contract.rs` |
