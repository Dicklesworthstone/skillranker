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
caps. No public Cloudflare request or quality acceptance is claimed here.
PR #8 remains closed; the integration is committed to main.
