use skillranker::eligibility::{Estimate, LoadedState, after_rerank};
use skillranker::identity::SkillId;
use skillranker::limits::DurationMillis;
use skillranker::roster::Visibility;
use skillranker::roster::discovery::claude_code_plan;
use skillranker::roster::resolution::resolve_claude_plan;
use skillranker::runtime::{EntryClock, ProcessInvocation};
use skillranker::scoring::{
    EPSILON, Input, MAX_TOP_K, Ranking, ScoringError, Weights, log_odds, rank, utility,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};

/// The documented formula, written independently of the implementation.
fn expected_utility(rerank: f64, fit: f64, prior: f64, phase: f64, w: (f64, f64, f64)) -> f64 {
    // A literal transcription of the documented min/max definition, kept
    // independent of the implementation's `clamp`.
    #[allow(clippy::manual_clamp)]
    let clip = |x: f64| (1.0 - EPSILON).min(EPSILON.max(x));
    clip(rerank).ln() + w.0 * (clip(fit) / (1.0 - clip(fit))).ln() + w.1 * prior + w.2 * phase
}

fn expected_scores(utilities: &[f64]) -> Vec<f64> {
    let max = utilities.iter().cloned().fold(f64::MIN, f64::max);
    let total: f64 = utilities.iter().map(|u| (u - max).exp()).sum();
    utilities.iter().map(|u| (u - max).exp() / total).collect()
}

fn ids(n: usize) -> Vec<SkillId> {
    (0..n)
        .map(|i| SkillId::new(format!("s_{i:03}")).unwrap())
        .collect()
}

fn input(id: &SkillId, rerank: f64, fit: f64) -> Input<'_> {
    Input {
        id,
        rerank,
        fit,
        prior_delta: 0.0,
        phase_match: 0.0,
    }
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-12
}

fn returned_ids<'a>(ranking: &Ranking, inputs: &[Input<'a>]) -> Vec<&'a SkillId> {
    ranking
        .returned
        .iter()
        .map(|s| inputs[s.index].id)
        .collect()
}

#[test]
fn documented_formula_is_recomputed_not_hard_coded() {
    let id = ids(3);
    let inputs = [
        input(&id[0], 0.50, 0.80),
        input(&id[1], 0.30, 0.60),
        input(&id[2], 0.25, 0.95),
    ];
    let ranking = rank(&inputs, Weights::DEFAULT, 3).unwrap();
    let utilities: Vec<f64> = inputs
        .iter()
        .map(|i| expected_utility(i.rerank, i.fit, 0.0, 0.0, (1.0, 0.0, 0.0)))
        .collect();
    let scores = expected_scores(&utilities);
    for scored in &ranking.returned {
        assert!(close(scored.utility, utilities[scored.index]));
        assert!(close(scored.rank_score, scores[scored.index]));
    }
    assert_eq!(ranking.omitted_mass, 0.0);
    // Prior and phase enter linearly with their weights.
    let weights = Weights::new(2.0, 0.5, 1.0).unwrap();
    let mut tuned = input(&id[0], 0.4, 0.7);
    tuned.prior_delta = -0.8;
    tuned.phase_match = 0.25;
    assert!(close(
        utility(&tuned, weights),
        expected_utility(0.4, 0.7, -0.8, 0.25, (2.0, 0.5, 1.0))
    ));
}

#[test]
fn a_singleton_scores_one_without_becoming_certain() {
    let id = ids(1);
    // Barely eligible: low fit and a rerank probability just above none.
    let inputs = [input(&id[0], 0.21, 0.31)];
    let ranking = rank(&inputs, Weights::DEFAULT, 5).unwrap();
    assert_eq!(ranking.returned.len(), 1);
    assert_eq!(ranking.returned[0].rank_score, 1.0);
    assert_eq!(ranking.omitted_mass, 0.0);
    assert_eq!(ranking.eligible, 1);
    // The raw estimates are untouched; the score says nothing about them.
    assert_eq!((inputs[0].rerank, inputs[0].fit), (0.21, 0.31));
}

#[test]
fn normalization_covers_every_eligible_candidate_before_top_k() {
    let id = ids(5);
    let inputs: Vec<Input<'_>> = id
        .iter()
        .zip([0.30, 0.25, 0.20, 0.15, 0.10])
        .map(|(id, p)| input(id, p, 0.7))
        .collect();
    let full = rank(&inputs, Weights::DEFAULT, 5).unwrap();
    let top = rank(&inputs, Weights::DEFAULT, 2).unwrap();
    assert_eq!(top.returned.len(), 2);
    assert_eq!(top.eligible, 5);
    // Top-K keeps the full-softmax scores rather than renormalizing them.
    assert_eq!(top.returned[..], full.returned[..2]);
    let kept: f64 = top.returned.iter().map(|s| s.rank_score).sum();
    assert!(kept < 1.0);
    assert!(close(kept + top.omitted_mass, 1.0));
    let dropped: f64 = full.returned[2..].iter().map(|s| s.rank_score).sum();
    assert!(close(top.omitted_mass, dropped));
    // Counterexample: normalizing over only the returned pair would differ.
    let renormalized = top.returned[0].rank_score / kept;
    assert!(renormalized - top.returned[0].rank_score > 1e-3);
}

#[test]
fn adding_an_eligible_candidate_changes_scores_by_the_formula() {
    let id = ids(4);
    let three: Vec<Input<'_>> = id[..3]
        .iter()
        .zip([0.5, 0.3, 0.2])
        .map(|(id, p)| input(id, p, 0.6))
        .collect();
    let mut four = three.clone();
    four.push(input(&id[3], 0.45, 0.9));
    let before = rank(&three, Weights::DEFAULT, 4).unwrap();
    let after = rank(&four, Weights::DEFAULT, 4).unwrap();
    let recompute = |inputs: &[Input<'_>]| {
        expected_scores(
            &inputs
                .iter()
                .map(|i| expected_utility(i.rerank, i.fit, 0.0, 0.0, (1.0, 0.0, 0.0)))
                .collect::<Vec<_>>(),
        )
    };
    let (e3, e4) = (recompute(&three), recompute(&four));
    let score = |r: &Ranking, index: usize| {
        r.returned
            .iter()
            .find(|s| s.index == index)
            .unwrap()
            .rank_score
    };
    for index in 0..3 {
        assert!(close(score(&before, index), e3[index]));
        assert!(close(score(&after, index), e4[index]));
        assert!(score(&after, index) < score(&before, index));
    }
}

#[test]
fn exact_ties_order_by_stable_id_in_any_input_order() {
    let a = SkillId::new("s_a").unwrap();
    let b = SkillId::new("s_b").unwrap();
    let c = SkillId::new("s_c").unwrap();
    for inputs in [
        [input(&b, 0.4, 0.6), input(&a, 0.4, 0.6)],
        [input(&a, 0.4, 0.6), input(&b, 0.4, 0.6)],
    ] {
        let ranking = rank(&inputs, Weights::DEFAULT, 2).unwrap();
        assert_eq!(returned_ids(&ranking, &inputs), vec![&a, &b]);
        assert_eq!(ranking.returned[0].rank_score, 0.5);
        assert_eq!(ranking.returned[1].rank_score, 0.5);
        // Top-1 of a tie keeps the smaller stable ID and reports the rest.
        let top = rank(&inputs, Weights::DEFAULT, 1).unwrap();
        assert_eq!(returned_ids(&top, &inputs), vec![&a]);
        assert_eq!(top.omitted_mass, 0.5);
    }
    // Changing the stable tie key changes the order, as declared.
    let renamed = [input(&c, 0.4, 0.6), input(&b, 0.4, 0.6)];
    let ranking = rank(&renamed, Weights::DEFAULT, 2).unwrap();
    assert_eq!(returned_ids(&ranking, &renamed), vec![&b, &c]);
    // A real score difference outranks the ID order.
    let unequal = [input(&a, 0.4, 0.6), input(&b, 0.4, 0.61)];
    let ranking = rank(&unequal, Weights::DEFAULT, 2).unwrap();
    assert_eq!(returned_ids(&ranking, &unequal), vec![&b, &a]);
}

/// Deterministic xorshift64* stream for seeded property cases.
struct Stream(u64);
impl Stream {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn unit(&mut self) -> f64 {
        // Mix exact extremes into otherwise uniform draws.
        match self.next() % 20 {
            0 => 0.0,
            1 => 1.0,
            2 => EPSILON / 2.0,
            _ => (self.next() >> 11) as f64 / (1u64 << 53) as f64,
        }
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[test]
fn seeded_properties_hold_for_generated_candidate_sets() {
    let pool = ids(MAX_TOP_K);
    let mut stream = Stream(0x5eed_5ca1_ab1e_0001);
    for case in 0..500 {
        let n = 1 + stream.below(MAX_TOP_K as u64) as usize;
        let top_k = 1 + stream.below(MAX_TOP_K as u64) as usize;
        let weights =
            Weights::new(stream.unit() * 4.0, stream.unit() * 0.5, stream.unit()).unwrap();
        let inputs: Vec<Input<'_>> = pool[..n]
            .iter()
            .map(|id| Input {
                id,
                rerank: stream.unit(),
                fit: stream.unit(),
                prior_delta: (stream.unit() - 0.5) * 20.0,
                phase_match: stream.unit(),
            })
            .collect();
        let ranking = rank(&inputs, weights, top_k).unwrap();
        assert_eq!(ranking.returned.len(), top_k.min(n), "case {case}");
        assert_eq!(ranking.eligible, n);
        let kept: f64 = ranking.returned.iter().map(|s| s.rank_score).sum();
        assert!(
            (kept + ranking.omitted_mass - 1.0).abs() < 1e-9,
            "case {case}"
        );
        for pair in ranking.returned.windows(2) {
            let (x, y) = (&pair[0], &pair[1]);
            assert!(x.rank_score >= y.rank_score, "case {case}");
            if x.rank_score == y.rank_score {
                assert!(inputs[x.index].id < inputs[y.index].id, "case {case}");
            }
        }
        for scored in &ranking.returned {
            assert!(scored.utility.is_finite() && scored.rank_score.is_finite());
            assert!((0.0..=1.0).contains(&scored.rank_score));
            let i = &inputs[scored.index];
            let w = (weights.fit(), weights.prior(), weights.phase());
            let expected = expected_utility(i.rerank, i.fit, i.prior_delta, i.phase_match, w);
            assert!((scored.utility - expected).abs() < 1e-9, "case {case}");
        }
        // Permutation equivalence: same stable IDs in the same order, same scores.
        let mut shuffled = inputs.clone();
        for i in (1..shuffled.len()).rev() {
            let j = stream.below(i as u64 + 1) as usize;
            shuffled.swap(i, j);
        }
        let permuted = rank(&shuffled, weights, top_k).unwrap();
        assert_eq!(
            returned_ids(&ranking, &inputs),
            returned_ids(&permuted, &shuffled),
            "case {case}"
        );
        for (x, y) in ranking.returned.iter().zip(&permuted.returned) {
            assert!(close(x.rank_score, y.rank_score), "case {case}");
        }
        assert!(close(ranking.omitted_mass, permuted.omitted_mass));
    }
}

#[test]
fn finite_extremes_and_zero_weights() {
    let id = ids(4);
    let max = Weights::new(4.0, 0.5, 1.0).unwrap();
    let mut extreme = vec![
        input(&id[0], 0.0, 0.0),
        input(&id[1], 1.0, 1.0),
        input(&id[2], 0.0, 1.0),
        input(&id[3], 1.0, 0.0),
    ];
    extreme[0].prior_delta = 1e6;
    extreme[3].prior_delta = -1e6;
    extreme[2].phase_match = 1.0;
    let ranking = rank(&extreme, max, 4).unwrap();
    assert!(
        ranking
            .returned
            .iter()
            .all(|s| s.utility.is_finite() && s.rank_score.is_finite())
    );
    assert!(close(
        ranking.returned.iter().map(|s| s.rank_score).sum(),
        1.0
    ));
    // Clipping bounds both extremes by ln((1 - eps) / eps). `1 - clip(x)` loses
    // precision near one, so the two ends are symmetric only to about 1e-10.
    let bound = ((1.0 - EPSILON) / EPSILON).ln();
    for x in [0.0, 1.0] {
        assert!(log_odds(x).is_finite() && log_odds(x).abs() <= bound + 1e-9);
    }
    assert!((log_odds(0.0) + log_odds(1.0)).abs() < 1e-9);

    // Zero weights reduce to Choice-only: scores are clipped probabilities over their sum.
    let zero = Weights::new(0.0, 0.0, 0.0).unwrap();
    let plain: Vec<Input<'_>> = id
        .iter()
        .zip([0.4, 0.3, 0.2, 0.1])
        .map(|(id, p)| input(id, p, 0.99))
        .collect();
    let ranking = rank(&plain, zero, 4).unwrap();
    let total: f64 = plain.iter().map(|i| i.rerank).sum();
    for scored in &ranking.returned {
        assert!(close(scored.rank_score, plain[scored.index].rerank / total));
    }
}

#[test]
fn weights_and_inputs_are_validated() {
    assert_eq!(Weights::default(), Weights::DEFAULT);
    assert_eq!(
        (
            Weights::DEFAULT.fit(),
            Weights::DEFAULT.prior(),
            Weights::DEFAULT.phase()
        ),
        (1.0, 0.0, 0.0)
    );
    assert!(Weights::new(4.0, 0.5, 1.0).is_ok());
    assert!(Weights::new(0.0, 0.0, 0.0).is_ok());
    for (fit, prior, phase) in [
        (4.000_001, 0.0, 0.0),
        (1.0, 0.500_001, 0.0),
        (1.0, 0.0, 1.000_001),
        (-1e-9, 0.0, 0.0),
        (f64::NAN, 0.0, 0.0),
        (1.0, f64::INFINITY, 0.0),
    ] {
        assert_eq!(
            Weights::new(fit, prior, phase),
            Err(ScoringError::InvalidWeight),
            "{fit} {prior} {phase}"
        );
    }
    let id = ids(2);
    let ok = input(&id[0], 0.5, 0.5);
    assert_eq!(
        rank(&[], Weights::DEFAULT, 1),
        Err(ScoringError::NoCandidates)
    );
    assert_eq!(
        rank(&[ok], Weights::DEFAULT, 0),
        Err(ScoringError::InvalidTopK)
    );
    assert_eq!(
        rank(&[ok], Weights::DEFAULT, MAX_TOP_K + 1),
        Err(ScoringError::InvalidTopK)
    );
    assert!(rank(&[ok], Weights::DEFAULT, MAX_TOP_K).is_ok());
    for bad in [1.5, -0.1, f64::NAN] {
        assert_eq!(
            rank(&[input(&id[0], bad, 0.5)], Weights::DEFAULT, 1),
            Err(ScoringError::InvalidEstimate)
        );
        assert_eq!(
            rank(&[input(&id[0], 0.5, bad)], Weights::DEFAULT, 1),
            Err(ScoringError::InvalidEstimate)
        );
    }
    let mut phase = ok;
    phase.phase_match = 1.2;
    let mut prior = ok;
    prior.prior_delta = f64::NAN;
    for bad in [phase, prior] {
        assert_eq!(
            rank(&[bad], Weights::DEFAULT, 1),
            Err(ScoringError::InvalidAdjustment)
        );
    }
    assert_eq!(
        rank(&[ok, input(&id[0], 0.2, 0.2)], Weights::DEFAULT, 2),
        Err(ScoringError::DuplicateCandidate)
    );
}

fn tree() -> std::path::PathBuf {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "sr-scoring-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&path).unwrap();
    path // Retained for inspection.
}

#[test]
fn scoring_only_orders_what_eligibility_admitted() {
    let workspace = tree();
    for name in ["alpha", "beta"] {
        let path = workspace.join(".claude/skills").join(name).join("SKILL.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            format!("---\nname: {name}\ndescription: d\n---\nbody\n"),
        )
        .unwrap();
    }
    let visibility = Visibility::Verified {
        contract_version: "test-adapter-v1".into(),
    };
    let plan = claude_code_plan(&workspace, None, visibility).unwrap();
    let clock = EntryClock::capture_with(
        DurationMillis::new("test_total", 30_000, 30_000).unwrap(),
        DurationMillis::new("test_cleanup", 200, 30_000).unwrap(),
    )
    .unwrap();
    let runtime = ProcessInvocation::from_clock(clock).unwrap();
    let cx = runtime.request_cx().unwrap();
    let roster = resolve_claude_plan(&plan, &BTreeMap::new(), &cx, &clock).unwrap();
    let shortlist: Vec<_> = roster.advisory().collect();
    let by_name = |name: &str| {
        shortlist
            .iter()
            .find(|s| s.binding.invocation.as_str() == name)
            .unwrap()
            .binding
            .id
            .clone()
    };
    // The documented case: fit blending would favor beta, but only alpha beats none.
    let estimates = BTreeMap::from([
        (
            by_name("alpha"),
            Estimate {
                fit: 0.31,
                rerank: 0.70,
            },
        ),
        (
            by_name("beta"),
            Estimate {
                fit: 0.999,
                rerank: 0.10,
            },
        ),
    ]);
    let outcome = after_rerank(
        &shortlist,
        &estimates,
        0.20,
        0.30,
        &BTreeSet::new(),
        LoadedState {
            branch: None,
            records: &[],
        },
    );
    let inputs: Vec<Input<'_>> = outcome.eligible.iter().map(Input::from_eligible).collect();
    // Even the strongest prior cannot add the excluded candidate: it is not an input.
    let ranking = rank(&inputs, Weights::new(4.0, 0.5, 1.0).unwrap(), 5).unwrap();
    assert_eq!(returned_ids(&ranking, &inputs), vec![&by_name("alpha")]);
    assert_eq!(ranking.returned[0].rank_score, 1.0);
    assert_eq!((inputs[0].rerank, inputs[0].fit), (0.70, 0.31));
}
