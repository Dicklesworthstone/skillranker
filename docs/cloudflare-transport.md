# Cloudflare-hosted Jev transport and provider selection

## Scope and setup

TypeSafe.ai's Jev remains the ranking engine. TypeSafe is the default hosting
provider; `provider.kind = "cloudflare"` in trusted user configuration or
`SR_PROVIDER=cloudflare` selects Cloudflare-hosted TypeSafe Jev. The environment
wins over user configuration. Project configuration cannot select a provider,
set either provider's credentials, or set a Cloudflare account.

Fresh Cloudflare ranking requires `CLOUDFLARE_ACCOUNT_ID` and
`CLOUDFLARE_API_TOKEN` in the process environment, plus the existing explicit
network consent (`--allow-network` or trusted `network.enabled`). A TypeSafe key
cannot satisfy the Cloudflare credential requirement. Selecting a provider or
supplying a credential never grants consent; offline and dry-run still block
network access. Hooks use the same configuration snapshot and selected credential.

Malformed unused `CLOUDFLARE_ACCOUNT_ID` or `CLOUDFLARE_API_TOKEN` values cannot
disable the TypeSafe path. Their sanitized validation errors remain in the frozen
environment snapshot and become errors if mutable trusted configuration later
selects Cloudflare. Duplicate definitions and unknown `SR_*` names remain errors
regardless of provider. Hook installation prerequisites name the selected
provider's credential and account requirements.

The default model is resolved after all layers: `jev-latest` for TypeSafe and
`typesafe/jev` for Cloudflare. An explicit `provider.model` or `SR_MODEL` is never
silently rewritten. Cloudflare currently accepts only `typesafe/jev`; an explicit
TypeSafe alias or another model is a configuration error. Remove an old explicit
model override or select `typesafe/jev` when switching to Cloudflare.

Missing account setup can still be inspected with `doctor --config` and does not
prevent local explicit skill resolution. It remains a native route: endpoint
construction fails before HTTP rather than falling back to TypeSafe. `doctor`
and `budget` require a valid account when inspecting the selected native route.

## One route through ranking, accounting and readiness

`EffectiveConfig::endpoint()` projects the selected route through the ranking
pipeline's existing endpoint interface. A native marker can only be created by
trusted configuration resolution, and `EndpointConfig::from_override` validates
the account and builds the fixed route. An arbitrary `TYPESAFE_ENDPOINT` string
cannot select the native protocol. That override affects TypeSafe only and cannot
redirect Cloudflare credentials or change the native request target.

`JevClient::new` selects the wire protocol from this validated endpoint, not from
a host-name guess. Both ranking stages therefore use the selected model,
credential and native encoder/decoder through the existing pipeline. No second
HTTP implementation, new runtime or fallback has been introduced.

The same canonical origin is used by ranking, `sr budget`, the shared attempt
allowance and the circuit breaker. Budget commands resolve the same effective
configuration as ranking, including a provider selected only in trusted user
configuration. Allowance and breaker scope remain per origin, so switching
Cloudflare accounts does not create a fresh allowance bucket. Cache and
single-flight request identity include the full validated target URL, including
the account path; distinct accounts cannot reuse each other's provider responses.
Equivalent hexadecimal account spellings normalize to lowercase.

Every retry revalidates provider admission. A changed effective provider or
account also withholds advisory publication, including a change during the final
request. Explicit locally resolved results remain independent of provider
selection. Environment and CLI values stay frozen during file revalidation;
file edits hidden by an environment override do not spuriously revoke a result.

Account IDs and tokens are absent from ordinary diagnostics. `doctor --config`
reports provider, selected model and presence only. Its legacy
`typesafe.endpoint.override_present` field indicates a resolved non-default
endpoint projection; for Cloudflare that includes the native route, not a
claim that the `TYPESAFE_ENDPOINT` environment variable supplied it. Use
`provider.kind` to identify which projection is active.

## Native wire contract

The native protocol follows the TypeSafe Jev AI Run contract:
<https://developers.cloudflare.com/ai/models/typesafe/jev/>.

- The production origin is fixed to `https://api.cloudflare.com`. Account IDs
  contain exactly 32 ASCII hexadecimal digits, normalized to lowercase.
- Requests use `/client/v4/accounts/{account_id}/ai/run` and
  `{model,input:{state,questions}}`, with model `typesafe/jev`.
- The complete transmitted request, including its native wrapper, must fit
  96 KiB and depth 64. Invalid requests are refused before the accounting
  callback or HTTP future is polled. Dry-run stages and evaluation preflight
  show/count these final native wire bytes through the same pure encoder used
  by HTTP. Evaluation admission compares the final wire digest to its preview.
  Logical canonical request bytes still identify cache and replay evidence;
  the existing full target URL keeps provider/account namespaces distinct.
- The strict native decoder checks the complete response envelope's byte limit,
  nesting and duplicate keys before extracting the result. Required model and
  usage come from the provider, not request aliases or fabricated zero counts.
- Returned answers still pass the common Jev question, option and probability
  validation. A malformed response never enters the cache as a valid answer.

Both protocols share verified TLS roots, origin-scoped credentials, disabled
redirects/proxies/internal retries, response header and body limits, cancellation
polling and the final post-decoding deadline check. A Wide request can reuse its
connection for Rerank; Rerank requests closure. Accounting calls the admission
callback once before HTTP. Failed or late started attempts retain unknown usage.

## Verification and qualification

`tests/provider_configuration.rs` adds selection, precedence, credential
isolation, account/target identity, no-fallback, policy revalidation and
no-consent regressions. `tests/config_contract.rs` keeps the independent trust
matrix and registry checks. `tests/cli_config.rs` includes the trusted-provider
allowance-scope regression. The native codec and shared client's existing
boundary/TLS tests continue to cover the underlying wire protocol.

The local Python TLS peer has a separate self-check:

```sh
python3 -B tests/fixtures/jev-tls/test_cloudflare_server.py
```

A Python peer check is not execution of the Rust client. The original authoring
container had no Rust toolchain; that limitation describes the initial authoring
step. The [September 29 deployment receipt](self-host-shadow-deployment.md)
records 1,532 default-feature Rust test passes and strict Clippy on the integrated
`5f71eac` source, with formatting recorded at `4244c14`. Those checks supersede
the original compile limitation, but do not establish public Cloudflare capacity,
availability, model quality, or warm-hook latency. TypeSafe-origin qualification
cannot establish those properties for a different provider origin.

The [current reality check](reality-check-bridge-plan.md) separates these gates.
`sr-sgca` owns malformed unused-provider environment isolation;
`sr-roadmap-l1i.5.30` owns final native wire-byte preview correctness;
`sr-roadmap-l1i.2.14` owns provider-specific qualification. Public checks require
separate explicit consent, synthetic or consented inputs, and attempt/runtime
caps. The dated qualification attempt below is separate from quality acceptance.
PR #8 remains closed; the integration is committed to main.

## October 4 public qualification attempt

The existing production-builder capacity probe now covers the native Cloudflare
envelope as well as TypeSafe. Wide contains 254 synthetic skills plus `__none__`;
rerank contains 32 synthetic candidates, their detailed fits, and `__none__`.
Both shapes include a 12,000-character synthetic task. The ordinary local test
checks the final wire-byte bounds and path-independent request identity for both
providers. These synthetic checks do not establish relevance or hook latency.

The ignored `budgeted_live_cloudflare_capacity_shapes` test requires
`SKILLRANKER_CLOUDFLARE_CAPACITY_CONSENT=1` before reading the explicitly exported
`CLOUDFLARE_ACCOUNT_ID` and `CLOUDFLARE_API_TOKEN`. A TypeSafe credential cannot
satisfy these prerequisites. The probe uses the production Asupersync client,
verified public TLS roots and fixed Cloudflare origin, at most two HTTP attempts,
no retries, and a 30-second budget with cleanup reserved. Compile without provider
credentials on the build worker; run the retrieved, hash-verified test executable
on the credential host. Never forward credentials through RCH. Missing consent,
missing credentials and invalid selected configuration fail the explicitly
selected test rather than becoming a skipped success.

On October 4, explicit authorization to complete `sr-roadmap-l1i.2.14` as part of
the remaining-task work authorized one bounded synthetic qualification run. The
remote-built Linux x86-64 test executable had SHA-256
`609e33e7ab25e5d99881cccd2ce9f9d155126b8ae3d48956bf3bf105b3094d13`.
Its source was `0a99ada94e40d377f495691750405c1b26ae788b` plus the single owned
`tests/jev_smoke.rs` overlay, RCH fingerprint
`217f9971757df22d556e4607e9ab8efea67f6c715744fd7c6a9acde23aa519b3`, using
locked dependencies and `nightly-2026-08-31`.

The public wide request used `typesafe/jev`, six questions and 61,041 native wire
bytes, with BLAKE3
`4189923ea76402f998eeca951bbfb8e15b03d218c4545f0fa9072a7dc78c7a89`.
Cloudflare returned HTTP 401 after 114 ms of the client attempt; the complete test
process took 411.477 ms and exited 101. One attempt was admitted and sent. Its
usage remains unknown; no returned model or validated response is available.
Rerank did not start. The run stopped without retrying or falling back to
TypeSafe, and no personal transcript, hook setting or production ledger was used.

The optional provider therefore remains **unqualified**. HTTP 401 establishes an
authentication refusal for this selected setup; it does not identify the precise
credential/account defect or establish a capacity limit. Refresh the trusted
credential setup before a separately capped run. A successful maximum-shape
receipt, provider-specific quality, representative availability and warm-hook
latency remain distinct gates; this failed attempt closes none of them.

On the same frozen source, the default-feature locked Linux x86-64 suite passed
1,548 tests with zero failures and 11 ignored entries. The ignored count includes
the new paid Cloudflare probe; its separately executed public failure above is
not a passing test. All-target `cargo check`, strict all-target Clippy on the
pinned nightly, formatting and documentation consistency also passed. Three
credential-free controls were re-executed locally against the retrieved binary
in a fresh solo review. That review was not independent verification.
The test-source SHA-256 was
`98065add796a78e0cf5f1441bc6f0cf9d86e0798e862a190d09a2246ee21825d`;
the unchanged `Cargo.lock` SHA-256 was
`fb5ede7791d786efb342afb33dacf073f303b7d91ce08ee1ad8e4009ec4032ea`.
UBS scanned the changed Rust test file and reported 19 critical panic-macro
findings, all test assertions or explicit failing prerequisite diagnostics.
They were reviewed without suppressing the category. These local results support
the implemented contract; they do not change the failed public disposition.

## October 4 credential repair and second attempt

Following explicit operator instruction to use `cf`, the maintainer setup now
has a protected, single-account API token with only Workers AI Read and Write
permissions. It expires on January 2, 2027. The ignored local environment file
remains owner-only; unrelated TypeSafe settings were preserved. Credentials and
account identifiers were not printed or forwarded to the build worker.
`cf ai models list` succeeded with this token. `cf auth whoami` also probes user
and account metadata endpoints outside these permissions, so its `tokenValid`
field cannot qualify or disqualify this narrowly scoped AI credential.

The second explicitly capped production-client run used the full-gated test
executable with SHA-256
`0a2d769f99efcccbeed303f7adaa968c273c7fdfa72171a6cb5e8612f00967f4`.
It was built from `a2a371af` plus frozen RCH source overlay
`391bd0a9809aa38bdca07574c8d696228efaa1dbe47507a6962b5b7104c2140f`.
All 408 tracked non-Beads files were compared with published revision
`e5b8258083f4355e10e9fa3f51925b7503d513a6` and were identical to that build's
source. Remote and retrieved executable hashes matched. A redundant rebuild
was terminated by SIGTERM after source synchronization; it supplies no test
proof. The existing full suite on the verified source passed 1,528 tests with
zero failures and 11 declared ignores after retirement of 20 duplicate
coordinator-layer tests. The public probe is a separate failing execution.

The same 61,041-byte, six-question maximum wide request and wire digest above
returned HTTP 402 after 3,442 ms of the client attempt. The complete process
took 3,886.940 ms and exited 101. Exactly one attempt was admitted and sent;
usage remains unknown, no returned model or validated answer is available,
and rerank did not start. There was no retry, TypeSafe fallback, personal
transcript, hook configuration change or production ledger effect.

The account's Workers Paid subscription was independently read using the saved
`cf` login. A paid Workers subscription therefore does not explain away this
failure. Cloudflare lists Jev as a
[third-party model](https://developers.cloudflare.com/ai/models/typesafe/jev/),
and [Unified Billing](https://developers.cloudflare.com/ai-gateway/features/unified-billing/)
requires prepaid inference credits. Insufficient credit or another billing
entitlement is the next setup issue to resolve; HTTP 402 alone does not prove
the precise cause. The billing-credit read was forbidden to the current login,
so no balance is asserted. The native model catalog returned no matching entry
and the schema endpoint returned 404; neither is evidence of a successfully
served Jev request. No new subscription or credit purchase was made.

The optional provider remains **unqualified** and `sr-roadmap-l1i.2.14` remains
open. Resolve billing access and entitlement before another separately capped
maximum-shape run. Authentication, billing readiness, actual provider capacity,
representative availability, judged quality and warm latency retain separate
dispositions.
