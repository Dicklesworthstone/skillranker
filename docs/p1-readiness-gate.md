# Phase P1 Acceptance Gate: Transport & Runtime Readiness (`sr-roadmap-l1i.2.12`)

## 1. Overview & Acceptance Declaration

This document records the completed verification and formal acceptance of Phase P1 (**Asupersync Transport and Typed Jev Contract**, boundary `p1_acceptance_gate`, bead `sr-roadmap-l1i.2.12`).

All Phase P1 deliverables and embedded invariants have been implemented, verified, and integrated into the SkillRanker codebase under Rust 2024 without Tokio, reqwest, or transparent ureq fallbacks.

---

## 2. Phase P1 Component Matrix & Evidence

| Roadmap ID | Boundary / Component | Test Suite / Artifact | Status |
|---|---|---|---|
| `sr-roadmap-l1i.2.1` | Asupersync owned runtime & monotonic deadlines | `tests/runtime_contract.rs` | Accepted |
| `sr-roadmap-l1i.2.2` | Bounded blocking leaves & cancellation | `tests/blocking_contract.rs` | Accepted |
| `sr-roadmap-l1i.2.3` | Trusted bounded subprocess execution | `tests/subprocess_contract.rs` | Accepted |
| `sr-roadmap-l1i.2.4` | Origin-scoped HTTPS endpoint canonicalization | `tests/endpoint_contract.rs` | Accepted |
| `sr-roadmap-l1i.2.5` | Strict wire codec (96 KiB req, 2 MiB resp) | `tests/jev_codec.rs` | Accepted |
| `sr-roadmap-l1i.2.6` | Distribution & argmax validation | `tests/jev_contract.rs` | Accepted |
| `sr-roadmap-l1i.2.7` | Transport security & public WebPKI roots | `tests/jev_transport.rs` | Accepted |
| `sr-roadmap-l1i.2.8` | Attempt budget accounting & single-use permits | `tests/jev_admission.rs` | Accepted |
| `sr-roadmap-l1i.2.9` | Classified retries & deadline-bound backoff | `tests/jev_retry.rs` | Accepted |
| `sr-roadmap-l1i.2.10` | Separately consented live Jev contract spike | `tests/jev_smoke.rs`, `docs/jev-contract-spike.md` | Accepted |
| `sr-roadmap-l1i.2.11` | End-to-end transport failure & shutdown matrix | `tests/transport_failures.rs`, `scripts/e2e/suites/transport.json` | Accepted |
| `sr-roadmap-l1i.2.12` | Phase P1 comprehensive acceptance gate | `tests/p1_gate.rs` | Accepted |

---

## 3. Core Invariants Verified

1. **Owned Runtime & Monotonic Clock (`EntryClock`):**
   - Entry deadline tracked monotonically from process startup.
   - Usable work budget strictly excludes the 500ms cleanup reserve.
   - Process invocation terminates and shuts down cleanly within the deadline.
2. **Endpoint Security & Routing:**
   - Canonical base origin joining `/v1/systemone` exactly once.
   - Strict rejection of insecure HTTP, non-root paths, credentials in URL, and query strings.
   - Credentials bound strictly to validated HTTPS origins.
3. **Codec & Distribution Integrity:**
   - Serialized requests bounded by 96 KiB; oversized requests fail at construction with `CodecError::TooLarge`.
   - Choice questions bounded by 255 options (`MAX_CHOICE_OPTIONS`); duplicate IDs rejected.
   - Response probabilities validated: sum within $\pm 10^{-4}$ tolerance of $1.0$, all values in $[0.0, 1.0]$, argmax option alignment strictly enforced.
4. **Privacy & Fail-Closed Admission:**
   - Unconsented or offline execution rejected locally before network initialization (`ProviderAdmissionRefusal::Offline`, `NetworkNotAuthorized`).
   - Missing credentials fail admission without attempting wire transfer.
   - Planted canary tokens never leaked into error messages, formatting, or logs.
5. **Attempt Allowance & Accounting:**
   - Invocation budget enforces at most 2 logical requests and 4 HTTP attempts total.
   - Single-use permits tracked monotonically; unknown usage preserved upon mid-flight failure.
6. **Classified Retries:**
   - 429, 529, 503 classified as retryable; 400, 401, 403, 404 classified as non-retryable.
   - `Retry-After` bounded by remaining deadline.
7. **Live Provider Contract:**
   - Live TLS 1.3 handshake with WebPKI trust anchors against `https://api.typesafe.ai`.
   - Alias `jev-latest` correctly resolved to versioned identifier `jev-1.13.0`.
   - Exact token usage accounting recorded.

---

## 4. Verification Receipts

```bash
# Phase P1 Acceptance Gate Test
rch exec -- cargo test --test p1_gate
# Output: test result: ok. 1 passed; 0 failed; finished in 0.03s

# Transport Failures Suite
rch exec -- cargo test --test transport_failures
# Output: test result: ok. 8 passed; 0 failed; finished in 2.96s

# Live Jev Smoke Suite
rch exec -- cargo test --test jev_smoke
# Output: test result: ok. 4 passed; 0 failed; finished in 0.50s

# E2E Transport Runner Mechanics Suite
scripts/e2e/run.sh --suite transport --artifacts /data/tmp/test-artifacts
# Output: {"product_gate":"not-applicable","run":"sr-e2e-ho872g5k","runner_status":"passed","schema_version":2}
```
