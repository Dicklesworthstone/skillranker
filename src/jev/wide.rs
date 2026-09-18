//! Wide selection questions and gate policy (P4). One request asks, over every
//! admitted candidate, which skill would most help the next step (with a typed
//! `__none__` sentinel), three oriented gate questions and the session phase.
//! The gate mean decides whether a detailed rerank runs. A winning sentinel
//! alone never ends the pass: the detailed rerank can rescue a poorly described
//! skill, so the best `M` real candidates are shortlisted either way.
//!
//! Every outgoing field is bounded: skill names and descriptions are redacted
//! before truncation, the serialized request is capped at 96 KiB, and the
//! assembled bytes are inspected again before they may be sent. Over budget,
//! older context and then description excerpts are trimmed in a fixed order;
//! admitted candidates and the sentinel are never dropped.

use super::codec::{Answer, CodecError, Question, Request, Response};
use crate::context::RenderedContextPayload;
use crate::identity::SkillId;
use crate::output::ErrorKind;
use crate::privacy::redaction::Redactor;
use crate::roster::WIDE_DESCRIPTION_MAX_SCALARS;
use crate::roster::resolution::{AdvisorySkill, OptionMap, ResolutionError, ResolvedRoster};
use serde_json::json;
use std::collections::BTreeMap;

pub const WIDE_POLICY_VERSION: &str = "wide-questions-v1";
pub const DEFAULT_MODEL: &str = "jev-latest";
pub const DEFAULT_GATE: f64 = 0.30;
pub const DEFAULT_TOP: usize = 5;
pub const DEFAULT_SHORTLIST: usize = 8;
pub const MAX_SIZE: usize = 32;
/// Real options per Choice; the sentinel makes 255.
pub const MAX_REAL_OPTIONS: usize = 254;
/// Decision reason when even a fully trimmed request cannot fit.
pub const REQUEST_TOO_LARGE: &str = "request-too-large";

pub const NONE_OPTION: &str = "__none__";
pub const WHICH: &str = "which";
pub const GATE_SPECIALIZED_METHOD: &str = "gate::specialized_method";
pub const GATE_MATERIAL_HELP: &str = "gate::material_help";
pub const GATE_CONTEXT_SUFFICES: &str = "gate::context_suffices";
pub const PHASE: &str = "phase";
pub const STUCK: &str = "stuck";

const WHICH_INSTRUCTIONS: &str = "Which available skill would most help the next step of this \
session? Planning, writing, analysis and explanation skills count; acting on files is not \
required. Choose __none__ when no listed skill adds useful guidance.";
const NONE_DESCRIPTION: &str = "No listed skill adds useful guidance for the next step.";
const SPECIALIZED_METHOD: &str =
    "Would the next step benefit from a specialized method, reference, or procedure?";
const MATERIAL_HELP: &str =
    "Would consulting a relevant skill materially improve correctness or execution here?";
const CONTEXT_SUFFICES: &str =
    "Is the existing context sufficient without consulting any additional skill?";
const PHASE_INSTRUCTIONS: &str = "Which phase best describes the next step of this session?";
const STUCK_INSTRUCTIONS: &str =
    "Is there evidence of repeated failure on the current task in this session?";
const PHASES: &[(&str, &str)] = &[
    ("planning", "Deciding what to do or how to approach it."),
    (
        "implementing",
        "Writing or changing code, content or configuration.",
    ),
    ("debugging", "Diagnosing or fixing a failure."),
    ("testing", "Writing, running or interpreting tests."),
    ("reviewing", "Examining existing work for problems."),
    ("releasing", "Packaging, publishing or deploying."),
    (
        "conversing",
        "Discussion or explanation without a concrete change.",
    ),
    ("other", "None of the above."),
];
/// Per-candidate description excerpt caps, tried in order under budget pressure.
const DESCRIPTION_CAPS: [usize; 4] = [WIDE_DESCRIPTION_MAX_SCALARS, 80, 40, 0];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WideError {
    /// `1 <= K <= M <= 32` failed before any adaptation to candidate counts.
    InvalidSizes,
    InvalidGate,
    NoCandidates,
    TooManyCandidates,
    IneligibleCandidate,
    /// Even with older context and excerpts trimmed, the request exceeds 96 KiB.
    RequestTooLarge,
    /// A field could not be redacted within its bounds.
    Redaction,
    /// The assembled payload still contains secret-shaped text; nothing is sent.
    Privacy,
    Codec(CodecError),
    /// A decoded response lacks a question this request asked.
    MissingAnswer,
}

impl WideError {
    pub const fn kind(self) -> ErrorKind {
        match self {
            Self::InvalidSizes | Self::InvalidGate => ErrorKind::InvalidConfiguration,
            Self::NoCandidates => ErrorKind::EmptyRoster,
            Self::TooManyCandidates | Self::IneligibleCandidate => ErrorKind::UnusableRoster,
            Self::RequestTooLarge => ErrorKind::OversizedInput,
            Self::Redaction | Self::Privacy => ErrorKind::NetworkDenied,
            Self::Codec(error) => error.kind(),
            Self::MissingAnswer => ErrorKind::InvalidProviderResponse,
        }
    }
}

/// Validated result sizes. `K` is returned; `M` is the shortlist.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Sizes {
    top: usize,
    shortlist: usize,
}

impl Default for Sizes {
    fn default() -> Self {
        Self {
            top: DEFAULT_TOP,
            shortlist: DEFAULT_SHORTLIST,
        }
    }
}

impl Sizes {
    /// Validate the configured values before any clamp to candidate counts.
    pub fn new(top: usize, shortlist: usize) -> Result<Self, WideError> {
        if (1..=shortlist).contains(&top) && shortlist <= MAX_SIZE {
            Ok(Self { top, shortlist })
        } else {
            Err(WideError::InvalidSizes)
        }
    }
    /// `(M_effective, K_effective)`. Fewer candidates than `M` is normal.
    pub fn effective(self, admitted: usize) -> (usize, usize) {
        let shortlist = self.shortlist.min(admitted);
        (shortlist, self.top.min(shortlist))
    }
}

pub fn validate_gate(gate: f64) -> Result<f64, WideError> {
    if (0.0..=1.0).contains(&gate) {
        Ok(gate)
    } else {
        Err(WideError::InvalidGate)
    }
}

/// What trimming the request needed to fit.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Trimming {
    /// Oldest recent messages removed; the latest request is always kept.
    pub dropped_messages: usize,
    /// The per-candidate description excerpt cap that fit.
    pub description_cap: usize,
    /// Description scalars omitted by that cap, across all candidates.
    pub omitted_description_scalars: usize,
    pub redactions: usize,
}

/// A built wide request: the exact bytes to send and the local option map.
pub struct WideRequest<'a> {
    request: Request,
    options: OptionMap<'a>,
    bytes: Vec<u8>,
    trimming: Trimming,
}

impl<'a> WideRequest<'a> {
    pub fn request(&self) -> &Request {
        &self.request
    }
    pub fn options(&self) -> &OptionMap<'a> {
        &self.options
    }
    /// The exact serialized request, as sent and as a dry run reports it.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn trimming(&self) -> Trimming {
        self.trimming
    }
}

fn noul(instructions: &str) -> Question {
    Question::Noul {
        instructions: json!(instructions),
        criteria: None,
    }
}

struct Descriptions {
    criteria: Vec<(String, String)>,
    omitted: usize,
    redactions: usize,
}

fn option_descriptions(options: &OptionMap<'_>, cap: usize) -> Result<Descriptions, WideError> {
    let redactor = Redactor::default();
    let mut omitted = 0;
    let mut redactions = 0;
    let mut criteria = Vec::with_capacity(options.entries().len() + 1);
    for (option, skill) in options.entries() {
        let name = redactor
            .redact_field(skill.binding.invocation.as_str())
            .map_err(|_| WideError::Redaction)?;
        redactions += name.redaction_count();
        let text = if cap == 0 {
            name.into_string()
        } else {
            let excerpt = redactor
                .redact_field_excerpt(skill.record.description_full.as_str(), cap)
                .map_err(|_| WideError::Redaction)?;
            redactions += excerpt.redaction_count();
            omitted += excerpt.omitted_scalars();
            format!("{}: {}", name.as_str(), excerpt.as_str())
        };
        if cap == 0 {
            omitted += skill.record.description_full.as_str().chars().count();
        }
        criteria.push((option.as_str().to_owned(), text));
    }
    criteria.push((NONE_OPTION.to_owned(), NONE_DESCRIPTION.to_owned()));
    Ok(Descriptions {
        criteria,
        omitted,
        redactions,
    })
}

fn assemble(
    model: &str,
    state: &RenderedContextPayload,
    criteria: Vec<(String, String)>,
    include_stuck: bool,
) -> Result<Request, CodecError> {
    let state = state.to_value().map_err(|_| CodecError::InvalidRequest)?;
    let mut questions = vec![
        (
            WHICH.to_owned(),
            Question::choice(json!(WHICH_INSTRUCTIONS), criteria)?,
        ),
        (GATE_SPECIALIZED_METHOD.to_owned(), noul(SPECIALIZED_METHOD)),
        (GATE_MATERIAL_HELP.to_owned(), noul(MATERIAL_HELP)),
        (GATE_CONTEXT_SUFFICES.to_owned(), noul(CONTEXT_SUFFICES)),
        (
            PHASE.to_owned(),
            Question::choice(
                json!(PHASE_INSTRUCTIONS),
                PHASES
                    .iter()
                    .map(|(key, text)| ((*key).to_owned(), (*text).to_owned())),
            )?,
        ),
    ];
    if include_stuck {
        questions.push((STUCK.to_owned(), noul(STUCK_INSTRUCTIONS)));
    }
    Request::new(model.to_owned(), state, questions)
}

/// Build the wide request over the admitted candidates. Explicit references
/// were resolved before this stage; an empty admission never reaches it.
pub fn build<'a>(
    roster: &'a ResolvedRoster,
    admitted: &[SkillId],
    state: &RenderedContextPayload,
    model: &str,
    include_stuck: bool,
) -> Result<WideRequest<'a>, WideError> {
    if admitted.is_empty() {
        return Err(WideError::NoCandidates);
    }
    if admitted.len() > MAX_REAL_OPTIONS {
        return Err(WideError::TooManyCandidates);
    }
    let options = OptionMap::new(roster, admitted).map_err(|error| match error {
        ResolutionError::Limit => WideError::TooManyCandidates,
        _ => WideError::IneligibleCandidate,
    })?;
    // Fixed order: drop the oldest messages first, then shorten excerpts.
    for cap in DESCRIPTION_CAPS {
        let Descriptions {
            criteria,
            omitted,
            redactions,
        } = option_descriptions(&options, cap)?;
        let mut trimmed = state.clone();
        let mut dropped = 0;
        loop {
            match assemble(model, &trimmed, criteria.clone(), include_stuck) {
                Ok(request) => {
                    let bytes = request.to_json().map_err(WideError::Codec)?;
                    Redactor::default()
                        .inspect_payload(&bytes)
                        .map_err(|_| WideError::Privacy)?;
                    return Ok(WideRequest {
                        request,
                        options,
                        bytes,
                        trimming: Trimming {
                            dropped_messages: dropped,
                            description_cap: cap,
                            omitted_description_scalars: omitted,
                            redactions,
                        },
                    });
                }
                Err(CodecError::TooLarge) if !trimmed.recent_messages.is_empty() => {
                    trimmed.recent_messages.remove(0);
                    dropped += 1;
                }
                Err(CodecError::TooLarge) => break,
                Err(error) => return Err(WideError::Codec(error)),
            }
        }
    }
    Err(WideError::RequestTooLarge)
}

/// One real candidate kept for the detailed rerank, with its raw wide value.
#[derive(Clone, Copy, Debug)]
pub struct Shortlisted<'a> {
    pub skill: AdvisorySkill<'a>,
    pub wide_probability: f64,
}

#[derive(Clone, Debug)]
pub enum WideDecision<'a> {
    /// `needs_skill` fell below the gate: `abstain / low-need`, no rerank.
    LowNeed,
    /// The best `M_effective` real candidates, highest raw probability first,
    /// ties by stable skill ID. The sentinel never takes a slot.
    Shortlist(Vec<Shortlisted<'a>>),
}

#[derive(Clone, Debug)]
pub struct WideOutcome<'a> {
    /// Mean of the three oriented gates; a heuristic, not a probability.
    pub needs_skill: f64,
    /// The sentinel's raw wide probability, kept as evidence.
    pub none_probability: f64,
    pub choice_confidence: f64,
    pub phase: BTreeMap<String, f64>,
    pub top: usize,
    pub decision: WideDecision<'a>,
}

fn noul_answer(response: &Response, key: &str) -> Result<f64, WideError> {
    match response.answers.get(key) {
        Some(Answer::Noul(value)) => Ok(*value),
        _ => Err(WideError::MissingAnswer),
    }
}

pub fn needs_skill(specialized_method: f64, material_help: f64, context_suffices: f64) -> f64 {
    (specialized_method + material_help + (1.0 - context_suffices)) / 3.0
}

/// Apply the gate and select the shortlist from a response already decoded
/// against this request, which guarantees its option set.
pub fn evaluate<'a>(
    wide: &WideRequest<'a>,
    response: &Response,
    gate: f64,
    sizes: Sizes,
) -> Result<WideOutcome<'a>, WideError> {
    let gate = validate_gate(gate)?;
    let needs = needs_skill(
        noul_answer(response, GATE_SPECIALIZED_METHOD)?,
        noul_answer(response, GATE_MATERIAL_HELP)?,
        noul_answer(response, GATE_CONTEXT_SUFFICES)?,
    );
    let Some(Answer::Choice(which)) = response.answers.get(WHICH) else {
        return Err(WideError::MissingAnswer);
    };
    let Some(Answer::Choice(phase)) = response.answers.get(PHASE) else {
        return Err(WideError::MissingAnswer);
    };
    let raw = which.raw_probabilities();
    let none_probability = raw
        .get(NONE_OPTION)
        .copied()
        .ok_or(WideError::MissingAnswer)?;
    let (shortlist_size, top) = sizes.effective(wide.options.entries().len());
    let decision = if needs < gate {
        WideDecision::LowNeed
    } else {
        let mut ranked = Vec::with_capacity(wide.options.entries().len());
        for (option, skill) in wide.options.entries() {
            let probability = raw
                .get(option.as_str())
                .copied()
                .ok_or(WideError::MissingAnswer)?;
            ranked.push(Shortlisted {
                skill: *skill,
                wide_probability: probability,
            });
        }
        ranked.sort_by(|a, b| {
            b.wide_probability
                .total_cmp(&a.wide_probability)
                .then_with(|| a.skill.binding.id.cmp(&b.skill.binding.id))
        });
        ranked.truncate(shortlist_size);
        WideDecision::Shortlist(ranked)
    };
    Ok(WideOutcome {
        needs_skill: needs,
        none_probability,
        choice_confidence: which.confidence(),
        phase: phase.raw_probabilities().clone(),
        top,
        decision,
    })
}
