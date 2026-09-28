# Configuration and authority contract (P0)

`src/config.rs` freezes the v1 configuration key registry, per-layer trust rules,
precedence, provenance and effective-policy receipts. `src/privacy/mod.rs` defines
invocation effect flags, network consent, the credential type, provider admission
and root containment. Both are pure: they read no files or environment, perform no
network or subprocess work and create no state. They do not implement TOML decoding,
clap parsing, `doctor --config`, origin canonicalization or effect enforcement.

## Layers and precedence

Ordinary values resolve `built-in → trusted user → project → environment → CLI`.
Each key declares one rule per layer: `forbidden`, `allowed`, or `restrict-only`
(may keep or narrow the value resolved from lower layers, never widen it). Lists
of exclusions and roster roots merge as unions, so a later layer can add entries
but cannot remove a trusted user's. A known key in a forbidden layer is an error,
not an ignored value. Reserved keys are recognized so an attempt is reported
precisely, but are settable in no layer in v1.

The command boundary supplies bounded entries: dotted paths for file and CLI
layers, and an environment snapshot. It must reject duplicate TOML keys during
decoding and finish this validation before discovery, networking or mutation.
Any issue invalidates the whole policy; up to 32 issues are listed, with the rest
counted. Diagnostics name layers and known registry keys, never values. Unknown
key names are discarded from diagnostic state and rendered as `<unknown key>`;
their text cannot appear in Display, Debug, or the public issue list.

| Key | Environment | CLI | User | Project | Env | CLI |
| --- | --- | --- | --- | --- | --- | --- |
| `network.enabled` | — | — | allowed | forbidden | — | — |
| `network.proxy` | — | — | reserved | reserved | — | — |
| `typesafe.api_key` | `TYPESAFE_API_KEY` | — | forbidden | forbidden | allowed | — |
| `typesafe.endpoint` | `TYPESAFE_ENDPOINT` | — | forbidden | forbidden | allowed | — |
| `provider.kind` | `SR_PROVIDER` | — | allowed | forbidden | allowed | — |
| `cloudflare.api_token` | `CLOUDFLARE_API_TOKEN` | — | forbidden | forbidden | allowed | — |
| `cloudflare.account_id` | `CLOUDFLARE_ACCOUNT_ID` | — | forbidden | forbidden | allowed | — |
| `provider.model` | `SR_MODEL` | — | allowed | forbidden | allowed | — |
| `hook.mode` | — | `--shadow` | allowed | forbidden | — | restrict-only |
| `hook.notification_turns` (`skip`\|`rank`) | — | — | allowed | restrict-only | — | — |
| `context.profile` | — | `--context-profile` | allowed | restrict-only | — | allowed |
| `context.no_tools` | — | `--no-tools` | allowed | restrict-only | — | restrict-only |
| `context.messages` (1–12) | `SR_MESSAGES` | `--messages` | allowed | restrict-only | allowed | allowed |
| `context.budget_chars` (1–12,000) | `SR_BUDGET_CHARS` | `--budget-chars` | allowed | restrict-only | allowed | allowed |
| `context.transcript_roots` | — | — | allowed (absolute) | forbidden | — | — |
| `ranking.top`, `ranking.shortlist` (1–32) | `SR_TOP`, `SR_SHORTLIST` | `--top`, `--shortlist` | allowed | allowed | allowed | allowed |
| `ranking.gate`, `ranking.fits` ([0,1]) | `SR_GATE`, `SR_FITS` | `--gate`, `--fits` | allowed | allowed | allowed | allowed |
| `ranking.w_fit` / `w_prior` / `w_phase` | — | — | allowed | allowed | — | — |
| `ranking.timeout_ms` (201–60,000) | `SR_TIMEOUT_MS` | `--timeout-ms` | allowed | forbidden | allowed | allowed |
| `ranking.exclude_skills` (≤128) | — | — | allowed | allowed | — | — |
| `roster.roots` (≤32) | — | — | allowed | allowed (contained) | — | — |
| `privacy.redaction`, `privacy.raw_retention` | — | — | reserved | reserved | — | — |

`provider.kind` is `typesafe` by default and also accepts `cloudflare`. Model
selection follows the winning provider after all layers resolve: an unspecified
model defaults to `jev-latest` for TypeSafe and `typesafe/jev` for Cloudflare.
An explicit model is never silently rewritten. Cloudflare currently accepts only
`typesafe/jev`; an explicit `jev-latest` or another route is a configuration error.
The account and token remain environment-only. Accounts must contain exactly
32 ASCII hexadecimal digits and normalize to lowercase. Missing account setup
can be inspected locally, but native endpoint construction fails before HTTP and
never falls back to TypeSafe. See [provider integration](cloudflare-transport.md).

Weights use the plan bounds `[0,4]`, `[0,0.5]` and `[0,1]`. Integer file values
are accepted for float keys; NaN and infinity are rejected everywhere. After
merging, `ranking.top` must not exceed `ranking.shortlist`. Disclosure-volume keys
are ordered for restriction: `minimal < standard`, `no_tools = true` is narrower,
and fewer messages or characters are narrower. `shadow < advisory` for hook mode.
`skip < rank` for `hook.notification_turns`. The default `skip` counts a turn
that a background task's `<task-notification>` started while the agent was idle
as a non-turn: it is harness output, not a user request, and is never sent.
`rank` evaluates it like a submitted prompt (sr-sif6).

The environment layer is strict inside `SR_`: an unrecognized `SR_*` variable or a
non-UTF-8 `SR_` name is an error, so a misspelled privacy setting is never ignored.
Other variables, including other `TYPESAFE_*` and `CLOUDFLARE_*` names, are outside
the schema. Environment values are parsed as `true`/`false`, unsigned decimal
digits, or Rust floating-point text. An empty provider token is absent; a token
with whitespace, controls or more than 4 KiB is a configuration error. The
selected provider uses only its own token; there is no cross-provider fallback.

Project roots must be relative and lexically contained in the workspace. Only
trusted user configuration may add an absolute root, which may not contain `..`.
Symlink containment is enforced when a reader opens the path. Transcript roots are
trusted-user-only and absolute. Each names a Claude-style projects directory that
bare `sr rank` session discovery searches in addition to `$HOME/.claude/projects`
(see [source selection](source-selection.md)).

## Effects, consent and admission

`EffectPolicy::from_flags` reports every conflict: `--offline` or `--dry-run` with
`--allow-network`, and `--save-case` with `--dry-run` or `--no-persist`. Dry run and
`--no-persist` disable the response cache, ledger and persistent runtime state
(keys, locks, leases, cooldowns and allowance accounting); `--no-cache` and
`--no-ledger` disable only their store. Offline blocks network access but still
permits cache reads and local explicit resolution. Ordinary configuration reads
remain allowed in every mode.

Network consent comes only from `--allow-network` or trusted `network.enabled`.
Offline and dry run block it even when trusted consent exists. A credential's
presence is never consent. `admit_provider_attempt` checks consent before the
credential, so a blocked or unauthorized run never reports a key problem. The
credential is a separate non-serializable value with redacted `Debug`; receipts
record only its presence.

Each failure maps onto the output schema's `ErrorKind` through `kind()`:
configuration issues are `invalid-configuration` and flag conflicts are
`invalid-usage` (both exit 2). A refused admission maps as follows: missing consent
or dry run gives `network-denied` (exit 8), and offline gives `cache-miss`
(exit 11). An offline refusal only arises when no complete valid cached result
exists. A missing selected-provider key gives `credential-absent` (exit 4).

## Receipts and revalidation

`ResolvedConfig::receipt` captures effective values, derived consent, credential
presence, effect flags, per-key source layers and a caller-supplied read generation.
`ResolvedConfig::revalidate` re-reads only the trusted user and project layers,
reusing this invocation's environment and CLI layers, and compares effective values
for one boundary's dependency projection:

| Boundary | Fields compared |
| --- | --- |
| Provider admission (every HTTP attempt) | consent, credential presence, provider, provider account, endpoint, model, profile, no-tools, messages, budget, transcript roots, shortlist, exclusions, roster roots |
| CLI advisory publication | provider, provider account, top, shortlist, gate, fits, weights, exclusions, roster roots |
| CLI explicit publication | roster roots |
| Hook advisory publication | CLI advisory fields plus hook mode |
| Hook explicit publication | roster roots, hook mode |

The invocation deadline is fixed at entry and appears in no projection. Network
consent does not gate local publication. Comments, file bytes and hidden edits
do not supersede advice: an edit overridden by a fixed CLI or environment value
leaves the effective value unchanged and is not reported as a revocation. Invalid
current configuration fails closed. A superseded result is withheld; the change
never authorizes a replacement request. Trusted advisory snoozes live beside
the user configuration in `sr/snoozes.toml`, outside this key registry: `sr
snooze --apply` alone writes them, and advisory publication rechecks the
decision's session scope, withholding advice that a newly applied snooze mutes
(`superseded`); an expiry only relaxes controls and supersedes nothing. The
optional shared attempt allowance is likewise a separate sr-managed trusted
file, `sr/allowance.toml`, outside this registry, so project configuration can
neither enable, raise nor disable it. Only `sr budget --apply` writes it, under
its own lock and with an activation intent (`state = "intent"`, then
`"ready"`). Its enforcement state is `allowance.sqlite3` in the private cache
directory. Later phases add trial and learned policy fields.

Allowance and breaker scopes remain per canonical origin. Cloudflare account
IDs distinguish full request targets and cache identities, not allowance buckets.
TypeSafe fingerprints retain their existing representation; unused Cloudflare
inputs do not invalidate a TypeSafe decision. Provider changes withhold advisory
publication as well as future attempts, but cannot veto local explicit results.

## Managed mutations

`ManagedPolicyMutation` validates calibration apply and rollback changes. Its
target is always trusted user configuration, and only `ranking.gate`,
`ranking.fits` and the three weights are writable. Authority, routing, disclosure,
hook, root and exclusion keys are rejected. The writer must preview first, compare
the recorded base digest with the file before an atomic replace, keep an
owner-only backup, and preserve every unrelated setting.

## Verification scope

`tests/config_contract.rs` checks registry consistency and bounds, and tests each
security-sensitive key at all four layers against a matrix written independently
of the registry. It also covers restrict-only widening with successful narrowing
counterparts, root containment, and that a key alone grants no consent while
trusted consent and `--allow-network` do. Further tests show that authority-shaped
fields in normalized input are rejected, that duplicate, unknown, NaN, infinite,
out-of-range and non-UTF-8 inputs are all reported together, and cover precedence,
all 128 flag combinations, boundary projections, fixed-layer edits, managed
mutations and private `Debug` output. `tests/provider_configuration.rs` covers
provider defaults, precedence, credential isolation, native endpoint projection,
account canonicalization and identity, no fallback, revalidation and managed
mutation restrictions. Authored tests are not execution evidence; see the
integration document for the exact validation limitations.
