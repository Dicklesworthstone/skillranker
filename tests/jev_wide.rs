use serde_json::{Value, json};
use skillranker::authorized_read::{AuthorizedRoot, AuthorizedRoots};
use skillranker::context::{RenderedContextPayload, RenderedMessage};
use skillranker::identity::{ContentHash, LogicalSkillKey, SkillId, SourceId};
use skillranker::jev::codec::MAX_REQUEST_BYTES;
use skillranker::jev::wide::{
    DEFAULT_GATE, NONE_OPTION, Sizes, WideDecision, WideError, WideRequest, build, evaluate,
    needs_skill,
};
use skillranker::limits::{DurationMillis, SKILL_FILE_BYTES};
use skillranker::output::ContextQuality;
use skillranker::privacy::ContextProfile;
use skillranker::roster::resolution::{BindingSpec, ResolvedRoster, SkillEntry};
use skillranker::roster::{InvocationName, InvocationRestrictions, Visibility};
use skillranker::runtime::{EntryClock, ProcessInvocation};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Exact bytes of the one-candidate request, reviewed when first pinned.
const GOLDEN_ONE: &[u8] = include_bytes!("fixtures/jev-wide-one.json");
/// Digest and length of the exact 254-candidate request bytes.
const GOLDEN_254_HASH: &str = "6a439f4bc7f2786af6d8535ccd13480c948f67a585b92af28bf91e6776c07bc6";
const GOLDEN_254_LEN: usize = 13_729;

fn tree() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "sr-wide-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&path).unwrap();
    path // Retained for inspection.
}

fn roster_with(root: &Path, descriptions: &[String]) -> ResolvedRoster {
    let clock = EntryClock::capture_with(
        DurationMillis::new("test", 30_000, 30_000).unwrap(),
        DurationMillis::new("cleanup", 500, 30_000).unwrap(),
    )
    .unwrap();
    let runtime = ProcessInvocation::from_clock(clock).unwrap();
    let cx = runtime.request_cx().unwrap();
    let roots = AuthorizedRoots::single(AuthorizedRoot::open_absolute(root).unwrap());
    let entries = descriptions
        .iter()
        .enumerate()
        .map(|(i, description)| {
            let name = format!("skill{i:03}");
            let file = format!("{name}.md");
            fs::write(
                root.join(&file),
                format!("---\nname: {name}\ndescription: {description}\n---\nBody.\n"),
            )
            .unwrap();
            SkillEntry::from_read(
                BindingSpec {
                    source: SourceId::new("fixture").unwrap(),
                    logical_key: LogicalSkillKey::new(name.as_str()).unwrap(),
                    invocation: InvocationName::new(name.as_str()).unwrap(),
                    priority: Some(1),
                    visibility: Visibility::Verified {
                        contract_version: "fixture-v1".into(),
                    },
                    restrictions: InvocationRestrictions {
                        agent_invocable: true,
                        user_invocable: true,
                    },
                },
                roots
                    .read_bounded(0, Path::new(&file), SKILL_FILE_BYTES)
                    .unwrap(),
            )
            .unwrap()
        })
        .collect();
    ResolvedRoster::resolve(entries, false, &cx, &clock).unwrap()
}

fn plain(n: usize) -> Vec<String> {
    (0..n)
        .map(|i| format!("Helps with task number {i}."))
        .collect()
}

fn ids(roster: &ResolvedRoster) -> Vec<SkillId> {
    roster.advisory().map(|s| s.binding.id.clone()).collect()
}

fn state(messages: Vec<RenderedMessage>, latest: &str) -> RenderedContextPayload {
    RenderedContextPayload {
        schema_version: 1,
        context_profile: ContextProfile::Standard,
        harness: "claude_code".into(),
        context_quality: ContextQuality::Complete,
        project_signals: Default::default(),
        session_state: Default::default(),
        recent_messages: messages,
        latest_user_request: latest.into(),
    }
}

fn message(text: &str) -> RenderedMessage {
    RenderedMessage {
        role: "user".into(),
        tool: None,
        status: None,
        summary: None,
        text: Some(text.into()),
    }
}

fn default_state() -> RenderedContextPayload {
    state(
        vec![message("Earlier: the parser fails on nested lists.")],
        "Refactor the parser so nested lists round-trip.",
    )
}

/// A provider answer shaped for this request: `which` probabilities by option,
/// the three gates, and a phase distribution.
fn respond(
    wide: &WideRequest<'_>,
    which: &BTreeMap<String, f64>,
    gates: (f64, f64, f64),
) -> skillranker::jev::codec::Response {
    let choice = which
        .iter()
        .max_by(|a, b| a.1.total_cmp(b.1).then_with(|| b.0.cmp(a.0)))
        .unwrap()
        .0
        .clone();
    let body = json!({
        "model": "jev-test",
        "answers": {
            "which": {"type": "choice", "choice": choice, "probabilities": which, "confidence": 0.5},
            "gate::specialized_method": {"type": "noul", "noul": gates.0},
            "gate::material_help": {"type": "noul", "noul": gates.1},
            "gate::context_suffices": {"type": "noul", "noul": gates.2},
            "phase": {"type": "choice", "choice": "implementing", "probabilities": {
                "planning": 0.1, "implementing": 0.5, "debugging": 0.1, "testing": 0.1,
                "reviewing": 0.05, "releasing": 0.05, "conversing": 0.05, "other": 0.05}, "confidence": 0.4}
        },
        "usage": {"input_tokens": 10, "output_tokens": 5}
    });
    wide.request()
        .decode_response(&serde_json::to_vec(&body).unwrap())
        .unwrap()
}

fn options(wide: &WideRequest<'_>) -> Vec<String> {
    wide.options()
        .entries()
        .keys()
        .map(|o| o.as_str().to_owned())
        .collect()
}

fn shortlist_names(outcome: &WideDecision<'_>) -> Vec<String> {
    match outcome {
        WideDecision::Shortlist(list) => list
            .iter()
            .map(|s| s.skill.binding.invocation.as_str().to_owned())
            .collect(),
        WideDecision::LowNeed => panic!("expected a shortlist"),
    }
}

#[test]
fn zero_and_overflowing_candidate_sets_build_no_request() {
    let root = tree();
    let roster = roster_with(&root, &plain(255));
    let all = ids(&roster);
    assert_eq!(all.len(), 255);
    assert_eq!(
        build(&roster, &[], &default_state(), "jev-latest", false).err(),
        Some(WideError::NoCandidates)
    );
    assert_eq!(
        build(&roster, &all, &default_state(), "jev-latest", false).err(),
        Some(WideError::TooManyCandidates)
    );
    assert_eq!(WideError::NoCandidates.kind().as_str(), "empty-roster");
}

#[test]
fn one_candidate_request_matches_its_golden_bytes() {
    let root = tree();
    let roster = roster_with(&root, &plain(1));
    let wide = build(
        &roster,
        &ids(&roster),
        &default_state(),
        "jev-latest",
        false,
    )
    .unwrap();
    assert_eq!(wide.bytes(), GOLDEN_ONE);
    let value: Value = serde_json::from_slice(wide.bytes()).unwrap();
    let criteria = value["questions"]["which"]["criteria"].as_object().unwrap();
    assert_eq!(criteria.len(), 2, "one skill plus the sentinel");
    assert!(criteria.contains_key(NONE_OPTION));
    assert_eq!(
        value["questions"].as_object().unwrap().len(),
        5,
        "stuck is optional and off"
    );
    // The same inputs always produce the same bytes.
    let again = build(
        &roster,
        &ids(&roster),
        &default_state(),
        "jev-latest",
        false,
    )
    .unwrap();
    assert_eq!(again.bytes(), wide.bytes());
}

#[test]
fn a_full_254_candidate_request_matches_its_golden_digest() {
    let root = tree();
    let roster = roster_with(&root, &plain(254));
    let wide = build(&roster, &ids(&roster), &default_state(), "jev-latest", true).unwrap();
    assert_eq!(wide.bytes().len(), GOLDEN_254_LEN);
    assert_eq!(
        ContentHash::from_bytes(wide.bytes()).as_str(),
        GOLDEN_254_HASH
    );
    let value: Value = serde_json::from_slice(wide.bytes()).unwrap();
    assert_eq!(
        value["questions"]["which"]["criteria"]
            .as_object()
            .unwrap()
            .len(),
        255
    );
    assert!(value["questions"]["stuck"].is_object());
    assert_eq!(wide.trimming().dropped_messages, 0);
}

#[test]
fn defaults_with_one_quill_hit_shortlist_and_return_one() {
    let root = tree();
    let roster = roster_with(&root, &plain(1));
    let wide = build(
        &roster,
        &ids(&roster),
        &default_state(),
        "jev-latest",
        false,
    )
    .unwrap();
    let sizes = Sizes::default();
    assert_eq!(sizes.effective(1), (1, 1));
    let only = options(&wide)[0].clone();
    let which = BTreeMap::from([(only, 0.7), (NONE_OPTION.to_owned(), 0.3)]);
    let outcome = evaluate(
        &wide,
        &respond(&wide, &which, (0.8, 0.8, 0.2)),
        DEFAULT_GATE,
        sizes,
    )
    .unwrap();
    assert_eq!(shortlist_names(&outcome.decision), vec!["skill000"]);
    assert_eq!(outcome.top, 1);
    assert_eq!(outcome.none_probability, 0.3);
}

#[test]
fn a_low_gate_mean_skips_the_rerank() {
    let root = tree();
    let roster = roster_with(&root, &plain(3));
    let wide = build(
        &roster,
        &ids(&roster),
        &default_state(),
        "jev-latest",
        false,
    )
    .unwrap();
    let mut which: BTreeMap<String, f64> = options(&wide).into_iter().map(|o| (o, 0.3)).collect();
    which.insert(NONE_OPTION.to_owned(), 0.1);
    // Even a confident skill choice cannot pass a low gate mean.
    let outcome = evaluate(
        &wide,
        &respond(&wide, &which, (0.1, 0.2, 0.9)),
        DEFAULT_GATE,
        Sizes::default(),
    )
    .unwrap();
    assert!(matches!(outcome.decision, WideDecision::LowNeed));
    // The gate decides; evaluation still sees the intrinsic top candidates.
    assert_eq!(outcome.intrinsic_shortlist.len(), 3);
    assert!((outcome.needs_skill - needs_skill(0.1, 0.2, 0.9)).abs() < 1e-12);
    assert!(outcome.needs_skill < DEFAULT_GATE);
    // The same answer passes a lower configured gate.
    let passes = evaluate(
        &wide,
        &respond(&wide, &which, (0.1, 0.2, 0.9)),
        0.1,
        Sizes::default(),
    )
    .unwrap();
    assert!(matches!(passes.decision, WideDecision::Shortlist(_)));
}

#[test]
fn a_winning_wide_sentinel_still_shortlists_for_rescue() {
    let root = tree();
    let roster = roster_with(&root, &plain(3));
    let wide = build(
        &roster,
        &ids(&roster),
        &default_state(),
        "jev-latest",
        false,
    )
    .unwrap();
    let o = options(&wide);
    let which = BTreeMap::from([
        (NONE_OPTION.to_owned(), 0.6),
        (o[0].clone(), 0.05),
        (o[1].clone(), 0.25),
        (o[2].clone(), 0.10),
    ]);
    let outcome = evaluate(
        &wide,
        &respond(&wide, &which, (0.7, 0.6, 0.3)),
        DEFAULT_GATE,
        Sizes::default(),
    )
    .unwrap();
    assert_eq!(outcome.none_probability, 0.6);
    assert_eq!(
        shortlist_names(&outcome.decision),
        vec!["skill001", "skill002", "skill000"]
    );
}

#[test]
fn the_shortlist_keeps_m_best_real_candidates_with_stable_ties() {
    let root = tree();
    let roster = roster_with(&root, &plain(12));
    let wide = build(
        &roster,
        &ids(&roster),
        &default_state(),
        "jev-latest",
        false,
    )
    .unwrap();
    let mut which: BTreeMap<String, f64> = options(&wide).into_iter().map(|o| (o, 0.08)).collect();
    which.insert(NONE_OPTION.to_owned(), 0.04);
    let outcome = evaluate(
        &wide,
        &respond(&wide, &which, (0.9, 0.9, 0.1)),
        DEFAULT_GATE,
        Sizes::new(3, 8).unwrap(),
    )
    .unwrap();
    let names = shortlist_names(&outcome.decision);
    assert_eq!(names.len(), 8);
    assert_eq!(outcome.top, 3);
    // Equal probabilities fall to the ascending stable skill ID.
    let mut expected: Vec<_> = roster
        .advisory()
        .map(|s| {
            (
                s.binding.id.clone(),
                s.binding.invocation.as_str().to_owned(),
            )
        })
        .collect();
    expected.sort();
    let expected: Vec<String> = expected.into_iter().take(8).map(|(_, n)| n).collect();
    assert_eq!(names, expected);
}

#[test]
fn sizes_and_gate_are_validated_before_adapting_to_candidates() {
    for (top, shortlist) in [(0, 8), (5, 33), (9, 8), (0, 0)] {
        assert_eq!(Sizes::new(top, shortlist), Err(WideError::InvalidSizes));
    }
    assert_eq!(Sizes::new(5, 8).unwrap(), Sizes::default());
    assert_eq!(Sizes::new(32, 32).unwrap().effective(254), (32, 32));
    assert_eq!(Sizes::default().effective(3), (3, 3));
    assert_eq!(Sizes::new(2, 8).unwrap().effective(3), (3, 2));
    let root = tree();
    let roster = roster_with(&root, &plain(1));
    let wide = build(
        &roster,
        &ids(&roster),
        &default_state(),
        "jev-latest",
        false,
    )
    .unwrap();
    let which = BTreeMap::from([
        (options(&wide)[0].clone(), 0.5),
        (NONE_OPTION.to_owned(), 0.5),
    ]);
    for gate in [-0.1, 1.1, f64::NAN] {
        assert_eq!(
            evaluate(
                &wide,
                &respond(&wide, &which, (0.5, 0.5, 0.5)),
                gate,
                Sizes::default()
            )
            .err(),
            Some(WideError::InvalidGate)
        );
    }
}

#[test]
fn budget_pressure_trims_older_context_then_excerpts_but_never_candidates() {
    let root = tree();
    let long: Vec<String> = (0..254)
        .map(|i| format!("Skill {i} {}", "detailed guidance ".repeat(20)))
        .collect();
    let roster = roster_with(&root, &long);
    let messages: Vec<RenderedMessage> = (0..12)
        .map(|i| message(&format!("message {i} {}", "context ".repeat(600))))
        .collect();
    let latest = "Keep this latest request intact.";
    let big = state(messages, latest);
    let wide = build(&roster, &ids(&roster), &big, "jev-latest", false).unwrap();
    let trimming = wide.trimming();
    assert!(
        trimming.dropped_messages > 0,
        "older context is trimmed first"
    );
    assert!(wide.bytes().len() <= MAX_REQUEST_BYTES);
    let value: Value = serde_json::from_slice(wide.bytes()).unwrap();
    assert_eq!(
        value["questions"]["which"]["criteria"]
            .as_object()
            .unwrap()
            .len(),
        255,
        "every admitted candidate and the sentinel remain"
    );
    assert_eq!(value["state"]["latest_user_request"], latest);
    // The oldest messages go first; the newest that fit remain, in order.
    let kept = value["state"]["recent_messages"].as_array().unwrap();
    assert_eq!(kept.len(), 12 - trimming.dropped_messages);
    if let Some(first) = kept.first() {
        let text = first["text"].as_str().unwrap();
        assert!(text.starts_with(&format!("message {} ", trimming.dropped_messages)));
    }
    let again = build(&roster, &ids(&roster), &big, "jev-latest", false).unwrap();
    assert_eq!(again.bytes(), wide.bytes(), "trimming is deterministic");

    // When even the latest request alone cannot fit, the request is refused.
    let huge = state(Vec::new(), &"x".repeat(MAX_REQUEST_BYTES));
    assert_eq!(
        build(&roster, &ids(&roster), &huge, "jev-latest", false).err(),
        Some(WideError::RequestTooLarge)
    );
    assert_eq!(
        WideError::RequestTooLarge.kind().as_str(),
        "oversized-input"
    );
}

#[test]
fn excerpts_shrink_only_after_all_older_context_is_gone() {
    let root = tree();
    let descriptions: Vec<String> = plain(254)
        .iter()
        .map(|d| format!("{d} {}", "more words ".repeat(20)))
        .collect();
    let roster = roster_with(&root, &descriptions);
    let messages: Vec<RenderedMessage> = (0..6)
        .map(|i| message(&format!("m{i} {}", "ctx ".repeat(500))))
        .collect();
    // A long latest request leaves room only once excerpts shrink.
    let latest = "y".repeat(60 * 1024);
    let wide = build(
        &roster,
        &ids(&roster),
        &state(messages, &latest),
        "jev-latest",
        false,
    )
    .unwrap();
    let trimming = wide.trimming();
    assert!(trimming.description_cap < 160, "excerpts had to shrink");
    assert_eq!(trimming.dropped_messages, 6, "all older context went first");
    let value: Value = serde_json::from_slice(wide.bytes()).unwrap();
    assert!(
        value["state"]["recent_messages"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        value["questions"]["which"]["criteria"]
            .as_object()
            .unwrap()
            .len(),
        255
    );
}

#[test]
fn descriptions_are_redacted_before_truncation_and_the_payload_is_inspected() {
    let root = tree();
    let secret = format!("ghp_{}", "a".repeat(36));
    // The token straddles the 160-scalar excerpt boundary.
    let description = format!("{} {secret} tail", "word ".repeat(30));
    let roster = roster_with(&root, &[description]);
    let wide = build(
        &roster,
        &ids(&roster),
        &default_state(),
        "jev-latest",
        false,
    )
    .unwrap();
    let text = String::from_utf8(wide.bytes().to_vec()).unwrap();
    assert!(!text.contains("ghp_"));
    assert!(wide.trimming().redactions >= 1);
    // Secret-shaped text in the state is refused rather than sent.
    let leaky = state(Vec::new(), &format!("please use {secret}"));
    assert_eq!(
        build(&roster, &ids(&roster), &leaky, "jev-latest", false).err(),
        Some(WideError::Privacy)
    );
}
