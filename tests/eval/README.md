# SkillRanker evaluation policy fixtures

These files are frozen contract artifacts for `sr-roadmap-l1i.1.8`. They define the v1 evaluation policy, representative synthetic cases, and deterministic expected values used to check that future evaluators preserve the intended semantics.

They are not benchmark results. They do not contain a 300-family relevance holdout, a 150-family controlled harm cohort, a 500-invocation operational cohort, live Jev responses, or production harness evidence.

Files:

- `evaluation_policy.v1.json` freezes the case model, split rules, common 0/1/2 loss, promotion thresholds, uncertainty assumptions, and missing/failed/unstarted semantics.
- `synthetic_cases.v1.jsonl` contains small representative oracle cases spanning positive, no-match, near-miss, multiple-valid, explicit, planning, loaded-reference, repeatable-workflow, overflow, compaction, roster-change, and operational-semantics boundaries.
- `expected_values.v1.json` contains hand-checkable loss, always-abstain, missing evidence, coverage, and harm-interval counterexamples.
- `../../scripts/validate_eval_policy.py` validates structure and recomputes deterministic examples using only the Python standard library.

Run:

```bash
python3 scripts/validate_eval_policy.py
python3 scripts/test_eval_policy.py
```

A passing validation means the contract artifacts are internally consistent. It does not claim that SkillRanker has passed any relevance, harm, operational, live-provider, or release gate.

The checker rejects duplicate definitions, invalid Unicode/JSON whitespace,
fractional or negative counts, numeric strings and booleans used as numbers.
It checks roster/oracle references, preserves attempted and unfinished case
denominators, and requires both positive and negative interval examples.
Regression tests retain synthetic files in a reported temporary directory for
diagnosis; they never read real sessions or call a provider.
