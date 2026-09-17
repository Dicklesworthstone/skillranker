# Narrow Quill dependency integration

`sr-roadmap-l1i.3.8` qualifies the shipping Quill API and enforces its dependency
boundary. `sr-roadmap-l1i.3.9` adds the bounded literal query compiler described
below. Roster retrieval (`.3.10`) and a working rank command remain separate work.

The manifest pins `frankensearch-quill` and `frankensearch-core` to
`39047c44c3a92ceb71d25c602913b8b2888e2fe7`, both with default features disabled.
The Asupersync patch pins `81fb7b579ce5f161622f1524391f5202a641cc2e`; its `Cx`
must be the same package type accepted by Quill's ingest, commit and search APIs.
Sources and license obligations remain in [dependency qualification](dependencies.md).
No upstream source or oracle tests are copied into this project.

## Dependency guard

Run `python3 scripts/check_dependency_graph.py`. It invokes locked `cargo tree`
without compiling, selecting normal, build and project dev edges for all target
platforms. Cargo metadata supplies both declared features and implicit optional
dependency features. For every subset it checks both default-enabled and
default-disabled builds. It rejects a feature matrix above 512 combinations
rather than silently sampling combinations. The current empty/default and `tui`
choices produce four explicit graph checks.

The guard verifies packages **and enabled features**. It refuses Tantivy,
alternate runtimes/HTTP clients, embedding engines, the FrankenSearch hybrid
facade and legacy lexical/oracle packages. Oracle and cass compatibility features
are denied even when they add no package. Quill and core must have no activated
optional/default/internal features, and share one immutable public Git revision.
Exactly one resolved Asupersync, Quill and core package identity is required.
Upstream dev dependencies that are not reachable from SkillRanker are not part of
its build; merely appearing as an optional package in a lockfile is not activation.

The JSON receipt records each command/feature set, dependency identities, enabled
features and manifest/lock/tree hashes. A manifest or lockfile change during the
audit invalidates the run. Graph coverage is not cross-platform compilation proof.
Guard regressions run with:

```bash
python3 -m unittest discover -s scripts -p test_dependency_graph.py -v
```

## Native expected-result fixtures

`tests/quill_contract.rs` uses `QuillIndex::in_memory` and the shipping
`index_documents`, `commit`, and `search_paginated` APIs. It uses a direct
Asupersync context, independent synthetic expected results, and no filesystem
index, network service, `fsfs`, private sibling configuration, or oracle backend.

The fixtures verify searchable title/content versus stored-only metadata,
title boosting, a static generated disjunction, stable-ID ingest and equal-score
cutoff at 254 of 257 matches, no exact-count work, cancellation even after a
successful cached query, typed fuel exhaustion, and invalid resource budgets.
Failure cases have successful counterparts. They are dependency integration
evidence; the later roster adapter must still validate untrusted inputs and
propagate the real invocation deadline.

Each build/commit and search emits a structured test event with case ID, stage,
engine version, elapsed microseconds, counts, and outcome. Events omit document
bodies and query strings. Timings are fixture observations, not hook latency or
hard RSS guarantees. Explicit fixture budgets constrain logical engine work;
allocation overhead remains outside those logical counters.

```bash
RCH_REQUIRE_REMOTE=1 rch exec -- cargo test --locked --test quill_contract -- --nocapture
```

Use a frozen source snapshot and the repository's required check/Clippy/test
gates before closing the bead. Revision-bound results are recorded in the live
bead; the commands above alone are not a passing verification claim.

## Bounded literal query compilation

`roster::retrieval::compile_query` accepts the invocation's `Cx` and three explicit
local text fields: latest request, active task, and recent errors. The caller
selects these fields under the context/disclosure policy, including `--no-tools`.
The compiler neither discovers context nor redacts it, and never sends it anywhere.

Policy `quill-literal-or-v1` admits up to 4,096 Unicode scalars from each source
before invoking Quill's shipping `FrankensearchDefault` analyzer. It examines only
one extra scalar to detect truncation. A partial final alphanumeric token is
discarded so truncation cannot invent a word prefix. Input truncation is reported
per source. Analyzer work and stored token data are bounded by these three source
prefixes, even when the caller supplies a much larger string.

Terms are interleaved in request/task/error order and deduplicated by their
normalized Quill token text. This preserves task and error evidence for terse
continuations. Each term is individually quoted, with quotes/backslashes escaped;
only the compiler supplies ` OR ` separators. The renderer uses the original
token spelling because Unicode lowercase expansion need not survive a second
tokenization unchanged (for example, dotted capital I). This still yields the
same analyzed term as document ingestion. No Boolean, field, range, glob, boost,
or whole-request phrase syntax comes from conversational text.

The output admits at most 128 distinct terms and 4,096 Unicode scalars, counting
quotes, escape characters and separators. Terms that do not fit are skipped and
counted, allowing a later shorter term to fit; candidates are never invented to
compensate. Validation requires the renderer's quoted-literal/OR grammar because
Quill's lenient parser can silently discard unquoted punctuation. The native
parser validates the completed string, and a structural
check requires exactly the expected terms, optional OR clauses, and the pinned
content/title fields and boosts (1 and 2). Any parser recovery, truncation, or
changed meaning is a typed failure. No analyzed terms returns an explicit empty
compilation; analyzed terms that cannot fit return an unrepresentable-query error.

Diagnostics expose counts and truncation flags. Input/query `Debug` output and
errors omit private text; the compiled query is intentionally not serializable.
The explicit `as_str()` accessor is for local Quill execution and must not be
logged. Cancellation is checked between bounded synchronous stages. Logical work
bounds do not establish a hard real-time or RSS guarantee.

`tests/quill_query.rs` exercises real native search for OR semantics, syntax
injection, multilingual normalization, exact size/term caps, context preservation,
empty input, zero matches, cancellation and fuel refusal with successful twins.
It logs structured case/count/timing events without query/document bodies.

```bash
RCH_REQUIRE_REMOTE=1 rch exec -- cargo test --locked --test quill_query -- --nocapture
```

The later roster adapter must call this compiler only for advisory overflow,
map empty compilation to `retrieval-empty`, propagate failures and truncation,
and resolve only actual Quill hits through its roster snapshot. This module does
not implement roster admission, explicit-resolution bypass, or an alternate engine.
