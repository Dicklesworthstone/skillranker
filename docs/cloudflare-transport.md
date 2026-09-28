# Cloudflare-hosted Jev transport boundary

## Scope

`JevClient::cloudflare` constructs an explicit library-level native transport.
`JevClient::cloudflare_with_environment_token` also validates a supplied token
and binds it to Cloudflare's fixed HTTPS origin. Neither constructor reads the
process environment, authorizes network traffic, or changes CLI configuration.

The CLI still selects TypeSafe under the existing product contract. This change
does not implement `SR_PROVIDER`, advertise Cloudflare CLI readiness, or merge
PR #8. Provider selection, shared allowance/readiness configuration and final
pipeline qualification remain separate integration gates. No new dependency,
runtime, HTTP fallback, implicit probe or GitHub Actions workflow is introduced.

## Native contract

The transport follows Cloudflare's documented TypeSafe Jev AI Run contract:
<https://developers.cloudflare.com/ai/models/typesafe/jev/>.

- The production origin is fixed to `https://api.cloudflare.com`. A 32-digit
  hexadecimal account ID is validated before constructing the fixed account
  path, with equivalent uppercase IDs normalized to lowercase.
- Requests use `/client/v4/accounts/{account_id}/ai/run` and
  `{model,input:{state,questions}}`. Only the documented `typesafe/jev` model
  route is accepted; a TypeSafe alias is not silently rewritten or transmitted.
- The 96 KiB request cap and depth-64 bound include the native wrapper. Invalid,
  oversized or excessively nested requests are rejected before the accounting
  callback or HTTP future is polled.
- Responses go through the complete-envelope size/depth/duplicate checks in
  `cloudflare_codec`, then the shared Jev answer validator. Missing model or
  usage is an error, never a requested-model fallback or a known-zero charge.
- `target_url()` returns the actual native target for request identity, not the
  unused TypeSafe path in the base-origin configuration. It contains an account
  identifier and must not be included in diagnostics or public receipts.

## Shared safety boundary

Both protocols use the same `JevTransport` implementation, verified TLS roots,
origin-scoped authorization, no-proxy/no-redirect/no-internal-retry HTTP client,
body/header limits, cancellation polling, final post-decode budget check and
stage-aware connection lifecycle. A Wide call can reuse its connection for
Rerank; Rerank requests closure. Callers still own attempt limits and retries.

Each accounted call invokes its callback once after local validation and before
polling HTTP. Callback refusal sends nothing. Malformed or late responses retain
`http_attempt_started = true`, so accounting must retain unknown usage rather
than record zero. A successful decode alone cannot publish or cache a response
that completed after cancellation or the work deadline.

## Qualification

Fourteen native boundary tests live under `jev::client::tests`, including the
exact byte and nesting limits, no-debit/no-send refusals and completion guards.
Ten tests under `jev::client::tests::tls` exercise the real native client and
`RetrySession` against an owned, bounded local TLS peer. These cover valid
native envelopes; both protocols' Wide/Rerank connection reuse; per-attempt
reauthorization; unknown usage on malformed answers and failed retries; header
and body limits; redirect and Retry-After policy; certificate and hostname
verification; and stalled-body cancellation/deadline closure.

The peer uses only the repository's public synthetic TLS certificates and token,
clears its environment, binds loopback on an ephemeral port and exits under a
30-second watchdog. The Rust owner kills and reaps it on failure. No real
credential, public-provider request or additional dependency is required.

An independent Python self-check verifies the peer's behavior, not the Rust
client. On 2026-09-28, this command passed 8 tests comprising 24 loopback TLS
scenarios in the authoring container, with certificate verification enabled:

```sh
python3 tests/fixtures/jev-tls/test_cloudflare_server.py
```

The Rust tests have **not** been compiled or executed in that container: Cargo,
rustc and rustfmt are absent. `git diff --check` passes, but is not a compiler or
formatting-gate substitute. The existing TypeSafe TLS, retry, admission,
configuration and pipeline suites plus the full locked suite must run on the
exact integrated revision before qualification. No live Cloudflare call or
provider-quality claim accompanies this implementation; CLI provider selection
and allowance/readiness integration remain incomplete.
