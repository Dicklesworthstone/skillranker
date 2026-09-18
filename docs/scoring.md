# Local scoring (P4)

`src/scoring.rs` orders the candidates that [local eligibility](eligibility.md)
admitted. It cannot add, restore or remove a candidate: it scores exactly the
inputs it is given, so priors, phase and fit blending cannot rescue a candidate
that failed eligibility.

## Formula

```text
eps = 1e-6
clip(x) = min(1 - eps, max(eps, x))
log_odds(x) = ln(clip(x) / (1 - clip(x)))

utility_i = ln(clip(p_rerank_i))
          + w_fit   * log_odds(fit_i)
          + w_prior * prior_delta_i
          + w_phase * phase_match_i

rank_score_i = exp(utility_i - max_utility) / sum_j exp(utility_j - max_utility)
```

`p_rerank_i` is the candidate's own raw rerank Choice probability. `phase_match_i`
is the phase distribution's mass over the skill's declared phases. Weights
default to `w_fit = 1`, `w_prior = 0` and `w_phase = 0`, and `Weights::new`
accepts only finite values in `[0, 4]`, `[0, 0.5]` and `[0, 1]`. Estimates must
lie in `[0, 1]`, prior deltas must be finite, and duplicate candidates are
rejected.

## Normalization and truncation

Normalization runs over every eligible candidate before top-K truncation. The
returned candidates keep their full-softmax scores. They are not renormalized, so
they can sum to less than one, and `omitted_mass` reports the remainder. `top_k`
must be in `1..=32`. A lone eligible candidate scores exactly 1. That makes it
the whole local ranking, not a certainly useful skill. Scores are relative local
weights, not calibrated probabilities, and the raw rerank and fit estimates are
left unchanged for output.

Candidates are ordered by descending score. Exact ties fall to the ascending
stable skill ID. Provider option handles are resolved before scoring and never
serve as tie keys, so input order cannot change the result.

## Verification

`tests/scoring_contract.rs` recomputes expected utilities and scores from an
independent transcription of the formula, rather than from recorded display
values. It covers:
- singleton semantics;
- full-softmax top-K mass, with a renormalization counterexample;
- a scores change when an eligible candidate is added;
- stable-ID tie twins in both input orders, including a renamed tie key;
- finite extremes at the weight limits;
- the zero-weight Choice-only reduction;
- weight and input validation;
- 500 seeded generated cases checking permutation equivalence, mass, ordering and utility;
- an end-to-end case where fit blending would favor a candidate that eligibility excluded.
