# Dependency qualification for SkillRanker

This document qualifies dependency sources for `sr-roadmap-l1i.1.7`. It is source and manifest evidence only. It does not import code, edit Cargo manifests, prove SkillRanker builds, or prove live TypeSafe/Jev interoperability.

The qualification was performed before the Cargo bootstrap existed. It did not
modify parent-owned implementation files. The subsequent bootstrap consumes the
pins below; its executed build evidence is recorded separately from this source
review.

## Recommended pins for parent-owned Cargo work

Use Git revisions, not local path dependencies, when the first public manifest is created:

```toml
[dependencies]
asupersync = { version = "0.5.0", default-features = false, features = ["runtime-core", "native-runtime", "tls-native-roots"] }
frankensearch-quill = { git = "https://github.com/Dicklesworthstone/frankensearch.git", rev = "39047c44c3a92ceb71d25c602913b8b2888e2fe7", default-features = false }
frankensearch-core = { git = "https://github.com/Dicklesworthstone/frankensearch.git", rev = "39047c44c3a92ceb71d25c602913b8b2888e2fe7", default-features = false }

[patch.crates-io]
asupersync = { git = "https://github.com/Dicklesworthstone/asupersync", rev = "81fb7b579ce5f161622f1524391f5202a641cc2e" }
```


FrankenSearch's workspace manifest depends on `asupersync = "0.5.0"` through `[workspace.dependencies]`, not through the local sibling checkout. The root patch is therefore load-bearing: it keeps Quill's public `Cx` argument type and SkillRanker's direct Asupersync type from coming from two different package sources. If parent chooses not to use `[patch.crates-io]`, run a duplicate-package check before writing any adapter; two `asupersync` sources are a blocker for passing `&Cx` into Quill APIs.

`asupersync` is currently at `81fb7b579ce5f161622f1524391f5202a641cc2e` locally. The plan note reviewed `6060c0a0d83a0692c15c95012bd814ab8ef4c529`, so use the current inspected revision above or deliberately re-run qualification if choosing the older rev. Quill matches the plan note at `39047c44c3a92ceb71d25c602913b8b2888e2fe7`.

For Asupersync, add `proc-macros` only if the implementation actually uses its `scope!`, `spawn!`, `join!`, or `race!` macros. `tls-webpki-roots` may replace or supplement `tls-native-roots` if a later transport bead chooses deterministic bundled roots and tests the exact configuration. Do not enable `tokio-compat`, `benchmark-adapters`, `metrics`, `fuzz`, `ci-cross-platform`, or `sqlite` for ordinary ranking hooks unless a later bead proves that specific need.

For Quill, keep `default-features = false`. Do not enable `tantivy-oracle`, `bench-internals`, `conformance-internals`, `profile-internals`, `pruning-conformance`, or `durability` for initial in-memory overflow retrieval. SkillRanker tests should use independent expected-result fixtures, not Quill's Tantivy oracle harness.

Crates.io publication of the exact FrankenSuite crate set was not verified here. The qualified non-local source is the public GitHub repo plus exact revision. No local path dependency belongs in the final public manifest. The Asupersync patch is a Git-source pin, not a local path dependency.

## Runtime source evidence

Asupersync `Cargo.toml` reports package `asupersync` version `0.5.0`, edition 2024, repository `https://github.com/Dicklesworthstone/asupersync`, and `LicenseRef-MIT-OpenAI-Anthropic-Rider`. Its `LICENSE` hash matches SkillRanker's rider license: `32a82e0a5754e72e51fae44b65a936c831c07376f21c90f5fb9e76897fcc3509`.

The inspected HTTP client source exposes an explicit client and `Cx` boundary: examples call `asupersync::http::Client::new().get(...).send(cx).await`, and the module states there is no process-global ambient client. `HttpClientBuilder::request_timeout` sets a total timeout that is meet-composed with the remaining `Cx` budget and any per-call timeout. TLS is feature-gated; roots come from `tls-native-roots`, `tls-webpki-roots`, or explicit added root certificates. The loopback `tests/http_client_https_e2e.rs` proves local TLS mechanics, not public-root TypeSafe endpoint interoperability.

Feature checks run against the current local Asupersync checkout:

- `cargo tree -p asupersync --no-default-features --features runtime-core,native-runtime,tls-native-roots -e normal -i tokio` returned success with nothing to print.
- The same feature set had no `reqwest` or `ureq` package in the feature graph.
- A feature-edge query can show `tokio` through Asupersync's dev/conformance dependency path; that is not a normal dependency for the recommended hook feature set.

Quill `Cargo.toml` reports package `frankensearch-quill` version `0.3.1`, edition and license-file inherited from the workspace, repository `https://github.com/Dicklesworthstone/frankensearch`. Its workspace `LICENSE` hash is also `32a82e0a5754e72e51fae44b65a936c831c07376f21c90f5fb9e76897fcc3509`.

The inspected Quill source supports the planned in-memory adapter:

- `QuillIndex::in_memory(QuillConfig)` constructs an owned-buffer index without filesystem I/O.
- `index_documents(&Cx, &[IndexableDocument]).await`, `commit(&Cx).await`, and `search_paginated(&Cx, query, limit, offset, exact_count)` are public shipping APIs.
- `IndexableDocument` has `id`, `content`, optional `title`, and metadata. Metadata is stored with results; it is not a searchable substitute for content.
- The default schema stores and indexes `content` as field 1 and `title` as field 2. The default parser searches both and boosts title by `2.0`.
- `QuillConfig` exposes bounded work controls including `query_fuel_budget`, `max_ingest_shards`, `deterministic_ingest`, and validation of zero or invalid budgets.

Feature checks run against the current local FrankenSearch checkout:

- `cargo tree -p frankensearch-quill --no-default-features -e features -i tantivy` returned no package match.
- The same no-default Quill graph returned no package match for `tokio`, `reqwest`, or `ureq`.
- `frankensearch-lexical` is optional and only enabled by Quill's `tantivy-oracle` feature. The source contains test/oracle references to Tantivy, so SkillRanker must not import those tests or enable that feature.

## Source-copy candidates from meta_skill

Do not depend on the `ms` application crate at runtime. The inspected source revision is `c9a616bcb29c89e640a95f2bca344c3053fdf7d0` from `https://github.com/Dicklesworthstone/meta_skill.git`.

`meta_skill/Cargo.toml` says `license = "MIT"`, but its `LICENSE` file is byte-identical to SkillRanker's MIT plus OpenAI/Anthropic rider. If any code is copied, preserve the actual rider text and record source path, revision, file hash, local modifications, and carried test provenance in `THIRD_PARTY_NOTICES.md`. Do not create that notices file just for this qualification bead; no source was imported.

Candidate slices:

| Source file | Hash | Permitted use | Required adaptation |
|---|---:|---|---|
| `src/security/secret_scanner.rs` | `0fdb1cefbd8b6df7175b0a6970424d816205697244af2412c9ba7b11ab7ca6d4` | Secret pattern ideas, overlap handling, regression cases | Remove secret previews from privacy-sensitive logs; add whole-payload and truncation-boundary tests |
| `src/core/spec_lens.rs` | `65bf35439f18a65d34d66b533655c678f22401c3a118e4b95a2f9f12a734c5ad` | Frontmatter/body parsing behavior and fence regressions | The current parser logs raw YAML on parse error; that must not enter hook diagnostics |
| `src/search/embeddings.rs` | `820673d045719e3cfd001399cb4e4b0a2c55c8d5ce5a5cdfbac91c0c84debff8` | Deterministic tokenization/hash-feature ideas only | Copy no API embedding backend or configuration |
| `src/suggestions/bandit/types.rs` | `e4cde0b2d7b3306b32850c8ee1e9828f16cb05ede0ec4ab47401027fcb7db6b1` | Reward bookkeeping examples | Do not reuse its signal-arm model as a per-skill usefulness prior |

## Evaluation-only candidates

These are not prerequisites for ordinary hook ranking.

`fsci-stats` from `frankenscipy` at `4f687085db83492704017b1d5ed7e26f646e9c41` exposes the interval and distribution surfaces named in the plan, including Wilson/Clopper-Pearson helpers and `BetaDist`. Its stats crate depends on several FrankenSciPy crates, so use it behind evaluation/reporting boundaries unless a later bead proves a narrower adapter.

`fnp-random` from `franken_numpy` at `7db69a9a764a38e4e050c46c2607a3be3ceea849` exposes `Generator::from_pcg64_dxsm`, `choice_indices`, `shuffle_slice`, and rejection-sampling `bounded_u64`. Its default features include `ndarray`, OS entropy, and Rayon; deterministic evaluation consumers should use `default-features = false` and provide explicit seeds.

`fp-join` and `fp-frame` from `frankenpandas` at `7ca8c602ecc4de7bd6f8d95828e52b383d95d7cd` expose `MergeValidateMode` and `merge_dataframes_on_with_options`. Keep them in offline report preparation unless later work proves the full frame stack is worth a runtime dependency. Any use must assert expected one-to-one or many-to-one cardinality, pre/post join counts, and unknown/null label handling.

## Toolchain and license boundary

The current shell default is `rustc 1.100.0-nightly (923c95cdf 2026-09-16)`, but the inspected sibling repos pin or have been reviewed around `nightly-2026-08-31`. SkillRanker's first Rust manifest should pin a dated nightly rather than inheriting a floating default.

Preserve SkillRanker's `LICENSE` verbatim. Use `LicenseRef-MIT-OpenAI-Anthropic-Rider` and `license-file = "LICENSE"` where package metadata needs a license declaration. Do not label copied FrankenSuite source as unmodified MIT.

## Checks performed

- Read current `AGENTS.md`, `README.md`, the comprehensive plan dependency sections, and live bead `sr-roadmap-l1i.1.7`.
- Inspected local Git heads, remotes, status, `Cargo.toml`, `LICENSE`, and toolchain files for Asupersync, FrankenSearch, meta_skill, FrankenSciPy, FrankenNumPy, FrankenPandas, FrankenTUI, and cass.
- Ran focused `cargo tree` feature checks for the recommended Asupersync and Quill feature sets.
- Verified sibling repos remained clean after inspection.

No TypeSafe/Jev or other live provider calls were made. No SkillRanker runtime code, Cargo files, or source imports were changed.
