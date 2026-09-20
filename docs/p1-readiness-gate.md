# Phase P1 Acceptance Gate: Transport & Runtime Readiness (`sr-roadmap-l1i.2.12`)

## 1. Acceptance status

Phase P1 acceptance is supported by:
1. **Live maximum-shape provider qualification** (`sr-roadmap-l1i.2.10`):
   CopperWren executed the authorized live re-check at `99de43e` under the
   0.1 distribution tolerance (`SKILLRANKER_CAPACITY_CONSENT=1`). The wide
   (60,521 bytes, 6 questions) + rerank (90,388 bytes, 33 questions) pair
   passed strict production decoding with 0 errors. See
   [the live qualification receipt](jev-contract-spike.md).
2. **Runtime constructor admission & bounded shutdown** (`sr-5n0b`):
   SilentFinch added bounded production CLI finishing and runtime constructor
   admission checks (`7d1dae5`, `241e228`, `ca94ad1`).
3. **Transport shutdown & accounting** (`sr-roadmap-l1i.2.11`):
   All 8 failure matrix unit/property tests and the E2E transport runner
   passed; task is closed.
4. **Comprehensive gate invariants** (`sr-roadmap-l1i.2.12`):
   `tests/p1_gate.rs` verified all 9 transport, runtime, codec, retry,
   accounting, and security invariants (`all_p1_invariants_verified` ok).

---

## 2. Phase P1 Component Matrix & Evidence

| Roadmap ID | Boundary / Component | Test Suite / Artifact | Status |
|---|---|---|---|
| `sr-roadmap-l1i.2.1` | Asupersync owned runtime & monotonic deadlines | `tests/runtime_contract.rs` | Focused suite; prior result requires source-bound receipt |
| `sr-roadmap-l1i.2.2` | Bounded blocking leaves & cancellation | `tests/blocking_contract.rs` | Focused suite; prior result requires source-bound receipt |
| `sr-roadmap-l1i.2.3` | Trusted bounded subprocess execution | `tests/subprocess_contract.rs` | Focused suite; prior result requires source-bound receipt |
| `sr-roadmap-l1i.2.4` | Origin-scoped HTTPS endpoint canonicalization | `tests/endpoint_contract.rs` | Focused suite; prior result requires source-bound receipt |
| `sr-roadmap-l1i.2.5` | Strict wire codec (96 KiB req, 2 MiB resp) | `tests/jev_codec.rs` | Focused suite; prior result requires source-bound receipt |
| `sr-roadmap-l1i.2.6` | Distribution & argmax validation | `tests/jev_contract.rs` | Focused suite; prior result requires source-bound receipt |
| `sr-roadmap-l1i.2.7` | Transport security & public WebPKI roots | `tests/jev_transport.rs` | Focused suite; prior result requires source-bound receipt |
| `sr-roadmap-l1i.2.8` | Attempt budget accounting & single-use permits | `tests/jev_admission.rs` | Focused suite; prior result requires source-bound receipt |
| `sr-roadmap-l1i.2.9` | Classified retries & deadline-bound backoff | `tests/jev_retry.rs` | Focused suite; prior result requires source-bound receipt |
| `sr-roadmap-l1i.2.10` | Separately consented live Jev contract spike | `tests/jev_smoke.rs`, `docs/jev-contract-spike.md` | Verified: live maximum-shape pair passed (99de43e receipt) |
| `sr-roadmap-l1i.2.11` | End-to-end transport failure & shutdown matrix | `tests/transport_failures.rs`, `scripts/e2e/suites/transport.json` | Focused suite; prior result requires source-bound receipt |
| `sr-roadmap-l1i.2.12` | Phase P1 comprehensive acceptance gate | `tests/p1_gate.rs` | Executed: all 9 P1 invariants verified |

---

## 3. Contract boundaries exercised by component tests

1. **Owned Runtime & Monotonic Clock (`EntryClock`):**
   - Entry deadline tracked monotonically from process startup.
   - Usable work budget excludes cleanup reserve: 200 ms by default; the gate test configures 500 ms.
   - Process invocation terminates and shuts down cleanly within the deadline.
2. **Endpoint Security & Routing:**
   - Canonical base origin joining `/v1/systemone` exactly once.
   - Strict rejection of insecure HTTP, non-root paths, credentials in URL, and query strings.
   - Credentials bound strictly to validated HTTPS origins.
3. **Codec & Distribution Integrity:**
   - Serialized requests bounded by 96 KiB; oversized requests fail at construction with `CodecError::TooLarge`.
   - Choice questions bounded by 255 options (`MAX_CHOICE_OPTIONS`); duplicate IDs rejected.
   - Response probabilities validated: sum within $\pm 0.1$ of $1.0$ and renormalized (the live provider returns totals such as 0.99), all values in $[0.0, 1.0]$, argmax option alignment strictly enforced.
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
7. **Live Provider Contract — incomplete:**
   - Explicitly selected smoke execution requires consent and an exported key.
   - A small successful request can establish that request's HTTPS and answer shape.
   - Provider capacity, negotiated TLS version, and an immutable model revision
     must not be inferred from local assertions or client configuration.

---

## 4. Historical command summaries (not current qualification receipts)

These earlier summaries lack exact source identity and do not certify the current
revision. In particular, the old smoke count cannot distinguish a live response
from its former no-key success branch. The E2E runner explicitly reports its
product gate as not applicable.

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

---

## 5. Test environment qualification (`sr-5n0b`)

Parts of this suite assert that `sr` completes real work inside its process
entry deadline. That deadline runs monotonically from process start
(`DEFAULT_INVOCATION_DEADLINE_MS` = 3000), so it includes the binary's own
startup, which `sr-5n0b` measured at 718-852 ms of runtime construction on an
otherwise idle worker and more on a busy one. Once startup approaches the
budget, `cli::timely` refuses the run with exit 6 before any work begins.

Such a refusal is correct product behavior, so that particular failure is not
evidence of a product defect, and a pass on a loaded machine is not evidence
that the timing assertions hold. A full-suite acceptance run therefore
qualifies only when its worker runs no concurrent foreign builds.

This qualification covers pre-admission refusals of that shape. It must not be
read as classifying every timing failure as environmental: a run that exceeds
its budget *after* admission, or that fails to drain within the cleanup
reserve, is a different class with its own open investigation, including
SilentFinch's finding that the production CLI never called bounded
`ProcessInvocation::shutdown` and relied on `Drop`.

| Condition | Source | Worker | Result |
|---|---|---|---|
| Concurrent foreign builds | `3a63db9` | `vmi1153651`, job 450 | 4 timing failures: `rank_acceptance::bare_rank_uses_the_only_session_of_this_workspace` (6, local inspection), `rank_inputs::a_transcript_longer_than_its_tail_window_reports_windowed_history` (6, roster validation), `subprocess_contract::descendant_holding_pipes_is_terminated_after_parent_exits` (`DeadlineExceeded`), `transport_failures::http_429_backoff_and_retry_after_budget_accounting` (stalled at 501.4 ms) |
| No foreign builds | `99de43e` | `vmi1149989`, `-j 5`, default test threads | 82 reports, 773 passed, 0 failed, 9 ignored, `[RCH] remote vmi1149989 (529.8s)` |

`501bbdc` removed the incidental cases: the two rank tests above examine
discovery and transcript quality, not the deadline, so they now request an
explicit 20 s budget. The tests that exist to prove deadline behavior keep
their tight budgets and are not exempt from this qualification:
`the_sr_binary_honors_a_shorter_timeout_flag` (800 ms) and
`the_sr_binary_honors_a_longer_configured_deadline` (15 s configured against a
3.5 s answer).

The two runs are not a controlled A/B, and the comparison is only partly clean.
`3a63db9..99de43e` changes exactly three files: `src/pipeline.rs` (the cass
dispatch arm) and the two test files above. So for the two rank tests the
budget change is a confound, and their pass at `99de43e` does not by itself
show that load caused the earlier failure. `subprocess_contract` and
`transport_failures` are untouched across those revisions, which makes their
red-on-loaded / green-on-quiet result consistent with load — consistent with,
not proof of, since an unreproduced intermittent failure has no established
cause either way.

Limits of this qualification: the green run above is the author's own, so it is
not independent verification; it does not show that `sr`'s startup cost is
acceptable inside a production hook budget, which is why `sr-5n0b` stays open;
and it changes no production deadline, capacity default, or assertion. Retire
this section once startup cost is measured as a small fraction of the smallest
test budget, or once no acceptance test depends on wall-clock margin.
