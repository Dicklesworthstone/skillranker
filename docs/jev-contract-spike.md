# Jev contract qualification (`sr-roadmap-l1i.2.10`)

## Scope and remaining gate

Qualification is incomplete. The live smoke test exercises one synthetic request
through the production Asupersync HTTPS client: a three-option Choice including
`__none__`, plus one Noul question. It does not establish the provider's maximum
context, criteria length, question count, or response size. The application caps
of 96 KiB/request, 2 MiB/decoded response and 255 Choice options are local limits,
not measurements of the service's capacity. P1 acceptance remains open until its
full live and transport requirements have revision-bound evidence.

Earlier versions of this document presented a response, token counts (361/58),
680 ms latency, TLS 1.3, and model `jev-1.13.0` as verified by this test. No
revision-bound execution receipt accompanied those claims. They are withdrawn as
qualification evidence. The separately recorded September 17 fixture has its own
provenance and does not certify this Asupersync transport.

## Explicit live execution

`budgeted_live_contract_smoke` is ignored in ordinary Rust test runs. Selecting it
explicitly requires `SKILLRANKER_LIVE_CONSENT=1` (or exactly `true`) and an exported
`TYPESAFE_API_KEY`. Missing prerequisites fail the selected test; they never turn
an unexecuted request into a passing live check. Consent is checked before the
credential lookup. The test never reads `.env` itself.

Build with RCH. Do not forward maintainer credentials to a build worker. Retrieve
the test executable and run it on the credential host after explicitly loading
the local credential as described in AGENTS.md. With tracing disabled and the
credential already exported, execute the retrieved binary:

```bash
SKILLRANKER_LIVE_CONSENT=1 /absolute/path/to/jev_smoke-test-binary \
  --ignored --exact budgeted_live_contract_smoke --nocapture
```

This reserves a single-use permit under a one-request/one-attempt allowance and
performs one direct request, with transport retries disabled, to
`https://api.typesafe.ai/v1/systemone`. It sends only the synthetic apple/carrot
example embedded in the test, never live session data. The test budget is
10,000 ms total with 500 ms reserved for cleanup; this is not the CLI default
of 3,000 ms with a 200 ms reserve. A successful receipt is printed only after
response assertions and runtime shutdown succeed.

The receipt records requested alias, a BLAKE3 digest of the returned model
identifier, request byte count and BLAKE3 digest, token usage, attempt count and
elapsed send time. Failed attempted calls preserve unknown usage. Hashing the returned
identifier avoids printing arbitrary provider-controlled text. Neither a model
alias nor a digest proves an immutable provider revision. The test does not
inspect the negotiated TLS version and makes no such claim.

## Offline regression coverage

The ordinary suite exercises seven tests: lazy consent-before-credential lookup;
actual subprocess selection without consent or a key; missing-consent and
missing-key provider admission; oversized request rejection; Choice option
bounds; offline admission; and large production-builder request shapes with
independent-fixture byte equality. Both live tests remain visibly ignored.
The subprocess regression requires failure with a static prerequisite diagnostic
and checks that a synthetic credential canary is absent from both output streams.

Run the ordinary suite without live credentials:

```bash
RCH_REQUIRE_REMOTE=1 rch exec -- cargo test --locked --test jev_smoke
```

An ordinary suite pass proves these local regressions, not live availability.
The real local TLS suite is `tests/jev_transport.rs`; runner-mechanics fixtures
in `scripts/e2e/suites/transport.json` are not provider qualification.

## Current execution evidence

On September 18, 2026, the final test executable passed both the ordinary suite
and one explicitly selected live request on the maintainer host. These are
self-executed checks, not independent review.

Source and executable identity:

- Base: `21f0b16336aa3b7fda58fedfa3ed14143d5c76a7`, with only
  `tests/jev_smoke.rs` overlaid by RCH; no peers' working changes included.
- RCH overlay fingerprint:
  `556f9ca918632095e639f5a30fc5fa23d4bf29a8d0435057ad94a757f1b4b468`.
- Smoke source SHA-256:
  `d4dca27e0d6044075a236503949dd30811644b4af6c37d211c6664c0cd3ef5bc`.
- Cargo.lock SHA-256:
  `fb5ede7791d786efb342afb33dacf073f303b7d91ce08ee1ad8e4009ec4032ea`.
- Retrieved Linux x86_64 executable SHA-256:
  `fb458631d441a89eb70ccb5df697227b14a61204d14fc84d8f1a14e913afa311`.
- Toolchain: `nightly-2026-08-31`; default Cargo features (empty), with the
  pinned Asupersync runtime/native-runtime/native-roots dependency features.

Remote `cargo test --offline --locked -j 1 --test jev_smoke`: **6 passed,
0 failed, 1 ignored, 0 filtered**. The retrieved executable repeated those six
ordinary tests locally with credentials and consent removed from its environment.
The explicit live selection then ran **1 passed, 0 failed, 0 ignored, 6 filtered**.
The six filtered tests are the ordinary tests, not missing live cases.
Remote `cargo clippy --offline --locked -j 1 --all-targets -- -D warnings` passed
on the same frozen source, as did `cargo check --offline --locked -j 1 --all-targets`.

The sanitized live receipt was:

```json
{
  "schema_version": 1,
  "kind": "synthetic-live-jev-smoke",
  "requested_model": "jev-latest",
  "returned_model_blake3": "852a1ee4113f64c6c68f982a6f528147eb6dcbd0cad8472774a34110c55ba5b5",
  "request_blake3": "bf7a416bd0dece8080d6d76fb075fd41e84d9d9fd48dfb872b2516731ce990f5",
  "request_bytes": 562,
  "questions": 2,
  "choice_options": 3,
  "admitted_attempts": 1,
  "http_attempts": 1,
  "input_tokens": 401,
  "output_tokens": 61,
  "elapsed_ms": 653,
  "provider_capacity_qualified": false
}
```

This single observation is not a latency percentile, quality result, full CLI
ranking run, TLS-version measurement, or provider-capacity guarantee. No session
content was sent and the credential remained on the maintainer host. Raw provider
bodies and credentials were not recorded. The original capacity requirements
remain on `.2.10`; `.2.12` and P1 remain open.

Maintainer run logs: `/data/tmp/sr-live-spike-final-tests.log`,
`/data/tmp/sr-live-spike-live.log`, and `/data/tmp/sr-live-spike-clippy.log`.


## Broader verification boundary

The full frozen-source offline test run exited 101 at `tests/roster_snapshot.rs`:
five snapshot cases returned storage failure. Its fixture makes the workspace
private but leaves its ancestor root subject to the worker umask; a writable
ancestor is refused by the production export policy. Inspection of all six retained
fixture trees confirmed ancestor mode 0775 and workspace mode 0700. This has been handed to the
owner of `sr-roadmap-l1i.3.17`; the security checks and assertions were not relaxed.
The full suite is **not passed**, and cases after the first failing test binary
were not executed. Log: `/data/tmp/sr-live-spike-full-tests.log`.

UBS static analysis of the edited test reports expected test panic/assert/unwrap
sites and two bounded test-data allocation loops (11 critical, 68 warnings).
These were reviewed as test-harness operations; no assertions were removed or
scanner suppressions added. UBS's Cargo phases were disabled to enforce remote
compilation; the actual check/Clippy/test results above are separate evidence.
Repository-wide formatting encountered peers' in-progress changes; the edited
smoke file and owned diff pass formatting/whitespace checks.

## Full-candidate qualification attempt — September 19, 2026 UTC

The larger qualification uses the actual wide/rerank builders over an authorized
synthetic roster. Inputs contain 254 skills, 1,000-character descriptions,
700-character bodies, and a 12,000-character synthetic request. The builders may
trim excerpts to fit the 96 KiB request cap. Ordinary tests check six questions
and 255 options in wide, and 33 questions/33 options in rerank. Both requests
exceed 40 KiB and remain within the application cap. Building the same synthetic
roster in different directories must produce identical provider request bytes.

Live execution is separately ignored and requires
`SKILLRANKER_CAPACITY_CONSENT=1` plus the exported key. It reserves at most two
requests/attempts, shares a 30-second transport deadline with 500 ms cleanup,
disables retries, and stops if wide fails. This is a qualification budget, not
the product's three-second default. Fixture preparation is outside the transport
measurement. The rerank shortlist is a fixed synthetic set, so this test is not
an end-to-end recommendation or ranking-quality evaluation.

```bash
SKILLRANKER_CAPACITY_CONSENT=1 /absolute/path/to/jev_smoke-test-binary \
  --ignored --exact budgeted_live_capacity_shapes --nocapture
```

**The live wide attempt failed:** `Response(InvalidDistribution)` from the strict
production decoder. One HTTP attempt started; token usage is unknown because no
validated response was returned. Rerank was not sent. There was no retry or
relaxation of the probability-sum tolerance. This establishes a compatibility
failure for the tested large request, not a measured provider capacity ceiling.
The error does not identify which distribution failed or its values; raw response
bodies were not captured. The smaller successful smoke result does not override
this failure. Capacity qualification and P1 acceptance remain open.

Executed source identity:

- Base `fd8133e77d9a31d5993cb7e29f35e9d29f97adba`, only smoke test overlaid.
- RCH overlay `d7ef247a55990a1014d8adb9fbea9d10f5e1e999f9362d92e9ca8ab2836c23be`.
- Test source SHA-256 `8b026f190734dbaa74d31dfdf293eddd11de129f04a4d83ca4fd8d4cac2e0826`.
- Binary SHA-256 `1a561fdf4399afcb058b6cf84c62c6fb37375e893b3330fd92fc8ba4c1cada1f`.
- Toolchain `nightly-2026-08-31`, default features, unchanged lockfile above.
- 25 focused tests passed remotely: seven rerank, seven smoke, eleven wide;
  both live tests ignored. Seven ordinary smoke tests also passed locally.
- Live selection: zero passed, one failed; eight other tests filtered out.
- Logs: `/data/tmp/sr-capacity-final-tests.log`, `/data/tmp/sr-capacity-local.log`,
  `/data/tmp/sr-capacity-live.log`.

The subsequent test-only revision adds a pre-send request-identity receipt so a
future failing attempt still records its request size/digest, plus the
independent-directory determinism assertion. It has no additional live evidence.
Strict Clippy on the frozen base failed before checking this test because of
three existing library warnings in `src/cache/coordination.rs` and `src/cli.rs`;
those peer-owned paths were not changed by this qualification.

Final offline verification of that subsequent revision passed the same 25 tests
with two live tests ignored (RCH overlay
`d5a39dd0f09e8b342c0a01abd95ac0c07a52a4fbbc662b037451de6ffbbf67d8`
on the same base). Log: `/data/tmp/sr-capacity-final-offline.log`.

The [TypeSafe API reference](https://docs.typesafe.ai/api), rechecked on
September 19, explicitly specifies that Choice probabilities sum to one. It does
not supply a reason to relax this project's `1e-4` tolerance. The failed response
needs bounded diagnostic evidence before attributing the discrepancy to provider
rounding, question count, or another cause.

The broader run reproduced the five roster fixture failures on the newer base.
Creating the synthetic snapshot fixture ancestor atomically with mode 0700 fixed
them without changing export policy or assertions. With that additional test-only
change, the **full offline Cargo test suite passed**, as did all-target Cargo
check, on base `fd8133e` plus overlay
`7912753efb222ad947b07281a822473eedd54be9d92504fe48e3aa13438d1dad`.
Logs: `/data/tmp/sr-capacity-roster-fixed-full.log` and
`/data/tmp/sr-capacity-check.log`. Live/installed-service tests remain explicitly
ignored in ordinary runs. This offline pass does not change the failed large
live request or the strict-Clippy limitation above.

### Numeric diagnosis of rejected distributions

`budgeted_live_distribution_diagnostic` is a separate, ignored, one-request
synthetic probe. It requires `SKILLRANKER_DIAGNOSTIC_CONSENT=1` and the exported
key, checked before fixture preparation. It sends the production wide request
through Asupersync with verified TLS, disabled redirects/retries/proxies, identity
encoding, and the same response-size bound. It is diagnostic instrumentation,
not a ranking transport or end-to-end qualification. No response body is saved.

The production decoder runs first. Only a valid response or its specific
`InvalidDistribution` rejection permits numeric inspection; malformed JSON,
duplicate keys, excess depth/bytes, and other decoder failures remain failures.
Output contains question ordinal, option count, sum, absolute drift, and whether
the sum meets the unchanged tolerance. It omits question/option names, provider
text, and credentials. A rejected response still fails the test, with usage
reported unknown. Ordinary tests exercise valid/invalid sums, duplicate-key
rejection, and the actual executable's refusal without explicit consent/key.

```bash
SKILLRANKER_DIAGNOSTIC_CONSENT=1 /absolute/path/to/jev_smoke-test-binary \
  --ignored --exact budgeted_live_distribution_diagnostic --nocapture
```

The September 19 diagnostic made **one request and passed strict decoding**:
60,521 request bytes, BLAKE3
`6f9f49869467cc0dfdd0198cdc83a57dc2dbed5b17742c35dae932b11d30c84d`.
The eight-option phase Choice (question ordinal 3) and 255-option skill Choice
(ordinal 5) each summed to exactly `1.0`, with zero measured drift. The selected
test finished in 1.15 seconds including synthetic fixture preparation and
shutdown; this is not an isolated HTTP latency measurement. No rerank or retry
was sent. This diagnostic does not extract usage/model identity, so those fields
remain unreported; no token-cost estimate is inferred.

This successful response **does not explain the earlier failed response**.
Neither failure frequency nor a reliable maximum-capacity claim follows from
these two observations. The earlier body was not retained, so its rejected sum
cannot be reconstructed. Large-request reliability, rerank qualification, and
P1 acceptance remain open; the tolerance is unchanged.

Executed identity and evidence:

- Base `d68fc9ff32845b7513d65e51eb3226e65f27ff17`, only `tests/jev_smoke.rs` overlaid.
- RCH overlay `6157a484a15152e4c787555fb52dccf1cf9468b93fb663e3ac55181ee6daec42`.
- Test SHA-256 `7685eccaf465891ba3e6b8aebcf9cc7447c6ea5b9dd1d9a2bbad9ce4685349b6`.
- Executable SHA-256 `f5c742ddb7276a64b845020c1cbde16320dc09e118fdbad2d06dadf682501de5`, matched on worker and maintainer host.
- Pinned `nightly-2026-08-31`, default features, locked offline remote build.
- Eight ordinary tests passed remotely and locally; three live tests ignored.
  The explicitly selected diagnostic passed once; ten other tests filtered out.
- Logs: `/data/tmp/sr-distribution-final-build.log`,
  `/data/tmp/sr-distribution-local.log`, `/data/tmp/sr-distribution-live.log`.
- Static UBS: zero critical findings; 206 warnings reviewed as test assertions,
  bounded fixtures/indexing, and test-generated JSON conversion. No suppressions.
