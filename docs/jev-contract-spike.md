# TypeSafe Jev Provider Contract Spike Evidence (`sr-roadmap-l1i.2.10`)

## 1. Executive Summary

This document records the exact results, schema shapes, token usage, and runtime limits observed during the bounded TypeSafe Jev contract spike (`sr-roadmap-l1i.2.10`, contract boundary `p1_jev_contract_smoke`).

The spike validates:
1. Public WebPKI TLS 1.3 handshake and endpoint canonicalization to `https://api.typesafe.ai/v1/systemone`.
2. Model alias resolution: requested alias `jev-latest` resolved to returned identifier `jev-1.13.0`.
3. Strict wire codec contracts: Choice questions with `__none__` sentinel and Noul questions.
4. Bounded byte and token accounting: request payload within 96 KiB budget, response within 2 MiB budget, and exact returned token usage (`input_tokens`, `output_tokens`).
5. Fail-closed admission: requests lacking explicit network consent (`NetworkConsent::NotAuthorized` or `NetworkConsent::Blocked`) or missing `TYPESAFE_API_KEY` are rejected before any network attempt or connection initiation.

---

## 2. Test Execution & Environment

- **Host Platform:** Linux (`x86_64`)
- **Endpoint Origin:** `https://api.typesafe.ai` (canonicalized to `/v1/systemone`)
- **TLS Version:** TLSv1.3 with public trust anchors (`/etc/ssl/certs/ca-certificates.crt`)
- **Runtime:** Asupersync owned task runtime with `EntryClock` and monotonic deadlines
- **HTTP Client:** Asupersync `HttpClient` (`no_redirects()`, `no_retries()`, `no_proxy()`, `max_connections_per_host(1)`)

---

## 3. Wire Protocol & Answer Shapes

### 3.1 Synthetic Request Payload (Sanitized)

```json
{
  "model": "jev-latest",
  "state": {
    "task": "bounded_contract_spike",
    "context": "Synthetic qualification probe. An apple is a fruit; a carrot is a vegetable. No real session data."
  },
  "questions": {
    "food_class": {
      "type": "choice",
      "instructions": "Which candidate is described as a fruit?",
      "criteria": {
        "apple": "An apple, sweet edible fruit produced by an apple tree",
        "carrot": "A carrot, root vegetable usually orange in color",
        "__none__": "Neither listed candidate fits the description"
      }
    },
    "fruit_health": {
      "type": "noul",
      "instructions": "Is an apple considered a healthy food?"
    }
  }
}
```

### 3.2 Live Response (Sanitized)

```json
{
  "model": "jev-1.13.0",
  "answers": {
    "food_class": {
      "type": "choice",
      "choice": "apple",
      "confidence": 1.0,
      "probabilities": {
        "apple": 1.0,
        "carrot": 0.0,
        "__none__": 0.0
      }
    },
    "fruit_health": {
      "type": "noul",
      "noul": 0.93
    }
  },
  "usage": {
    "input_tokens": 361,
    "output_tokens": 58
  }
}
```

### 3.3 Validated Invariants

- **Model Separation:** `requested_model` (`jev-latest`) vs `returned_model` (`jev-1.13.0`).
- **Choice Answer Structure:**
  - `choice` matches argmax of probabilities (`apple`).
  - `confidence` is finite within $[0.0, 1.0]$ (`1.0`).
  - Probability distribution sums to $1.0$ within $\pm 10^{-4}$ tolerance.
  - Criteria keys match exactly with no foreign or omitted options.
- **Noul Answer Structure:**
  - `noul` is finite within $[0.0, 1.0]$ (`0.93`).
- **Usage Accounting:**
  - Non-zero token usage recorded: `input_tokens: 361`, `output_tokens: 58`, `total_tokens: 419`.
  - Latency: ~680ms under production TLS.

---

## 4. Boundary Tests

The test suite in `tests/jev_smoke.rs` exercises 4 cases:
1. `budgeted_live_contract_smoke`: Live consented call against TypeSafe Jev API when credentials and consent are present; verifies fail-closed admission when absent.
2. `request_size_and_limits_bounded`: Verifies that requests exceeding `MAX_REQUEST_BYTES` (96 KiB) fail during construction with `CodecError::TooLarge`.
3. `choice_options_limit_and_none_sentinel`: Verifies that Choice questions permit up to 255 options (`MAX_CHOICE_OPTIONS`) and reject 256 options with `CodecError::InvalidRequest`.
4. `unauthorized_attempt_refused_without_network`: Verifies that `NetworkConsent::Blocked(NetworkBlock::Offline)` prevents HTTP attempt from starting (`!http_attempt_started`), returning `ProviderAdmissionRefusal::Offline`.

---

## 5. Verification Commands

```bash
# Unit / Property Test Suite
rch exec -- cargo test --test jev_smoke

# Formatting and Clippy
cargo fmt --check -- tests/jev_smoke.rs
rch exec -- cargo clippy --locked --all-targets -- -D warnings

# Transport E2E Suite (includes consented-live-smoke)
scripts/e2e/run.sh --suite transport --artifacts /data/tmp/test-artifacts
```
