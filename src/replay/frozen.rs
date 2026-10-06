//! Versioned frozen inputs for opt-in replay. Digests prove consistency only.
use super::{ReplayCase, ReplayError};
use crate::identity::{ContentHash, SkillId};
use crate::jev::{
    codec::{Answer, Question, Request, RequestFormat, Response},
    rerank, wide,
};
use crate::privacy::redaction::Redactor;
use crate::roster::resolution::OptionMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use std::io::Write;

pub const FORMAT_VERSION: u64 = 1;
pub const COMPUTATION_VERSION: &str = "frozen-scoring-v1-disabled-prior-phase";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NumericProfile {
    pub backend: String,
    pub target: String,
    pub build: String,
    pub dependency_digest: String,
    pub computation_digest: String,
    pub probe_digest: String,
}
impl NumericProfile {
    pub fn current() -> Self {
        let mut probes = Vec::new();
        for value in [0.0_f64, 1e-6, 0.1, 0.3, 0.5, 0.9, 1.0 - 1e-6, 1.0] {
            probes.extend_from_slice(&crate::scoring::log_odds(value).to_bits().to_le_bytes());
            probes.extend_from_slice(&crate::scoring::clip(value).ln().to_bits().to_le_bytes());
            probes.extend_from_slice(&value.exp().to_bits().to_le_bytes());
        }
        Self {
            backend: "rust-native-f64-ln-exp-v1".into(),
            target: format!(
                "{}-{}-{}",
                std::env::consts::ARCH,
                std::env::consts::OS,
                if cfg!(target_env = "musl") {
                    "musl"
                } else if cfg!(target_env = "gnu") {
                    "gnu"
                } else {
                    "native"
                }
            ),
            build: format!(
                "{};tui={};compiler-controls={}",
                if cfg!(debug_assertions) {
                    "debug"
                } else {
                    "release"
                },
                cfg!(feature = "tui"),
                env!("SKILLRANKER_NUMERIC_BUILD_DIGEST")
            ),
            dependency_digest: digest(
                &[
                    include_bytes!("../../Cargo.lock").as_slice(),
                    include_bytes!("../../rust-toolchain.toml").as_slice(),
                ]
                .concat(),
            ),
            computation_digest: env!("SKILLRANKER_REPLAY_COMPUTATION_DIGEST").into(),
            probe_digest: digest(&probes),
        }
    }
}

/// Ordered local membership; no load path, session identity or cache key.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenMember {
    pub skill_id: String,
    pub content_hash: String,
    pub visibility: String,
    pub pre_fit_eligible: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenScoreInput {
    pub skill_id: String,
    pub prior_delta: f64,
    pub phase_match: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenOption {
    pub option_id: String,
    pub skill_id: String,
    pub content_hash: String,
}

/// Canonical logical request and validated response; raw probability rounding is retained.
/// A missing response is an unobserved stage, never an invented failure answer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenStage {
    pub stage: String,
    pub questions_version: String,
    pub request_json: String,
    pub wire_request_json: String,
    pub request_format: String,
    pub wire_version: String,
    pub options: Vec<FrozenOption>,
    pub response_json: Option<String>,
    pub privacy_transformed: bool,
}
impl FrozenStage {
    pub fn capture(
        stage: &str,
        request: &Request,
        options: &OptionMap<'_>,
        provider: &str,
    ) -> Result<Self, ReplayError> {
        let bytes = request
            .to_json()
            .map_err(|_| invalid("capture request is invalid"))?;
        let mut value: Value =
            serde_json::from_slice(&bytes).map_err(|_| invalid("capture request is invalid"))?;
        let mut privacy_transformed = redact(&mut value)?;
        let request_json = if privacy_transformed {
            serde_json::to_string(&value).map_err(|_| invalid("capture encoding failed"))?
        } else {
            String::from_utf8(bytes).map_err(|_| invalid("capture encoding failed"))?
        };
        let format = provider_format(provider)?;
        let wire = request
            .to_wire_json(format)
            .map_err(|_| invalid("capture wire request is invalid"))?;
        let mut wire_value: Value = serde_json::from_slice(&wire)
            .map_err(|_| invalid("capture wire request is invalid"))?;
        let changed = redact(&mut wire_value)?;
        privacy_transformed |= changed;
        let wire_request_json = if changed {
            serde_json::to_string(&wire_value)
                .map_err(|_| invalid("capture wire encoding failed"))?
        } else {
            String::from_utf8(wire).map_err(|_| invalid("capture wire encoding failed"))?
        };
        Ok(Self {
            stage: stage.into(),
            questions_version: if stage == "wide" {
                wide::WIDE_POLICY_VERSION
            } else {
                rerank::RERANK_POLICY_VERSION
            }
            .into(),
            request_json,
            wire_request_json,
            request_format: provider.into(),
            wire_version: "jev-provider-wire-v1".into(),
            options: options
                .entries()
                .iter()
                .map(|(id, skill)| FrozenOption {
                    option_id: id.as_str().into(),
                    skill_id: skill.binding.id.as_str().into(),
                    content_hash: skill.record.source_content.as_str().into(),
                })
                .collect(),
            response_json: None,
            privacy_transformed,
        })
    }
    pub fn record_response(&mut self, response: &Response) -> Result<(), ReplayError> {
        let mut value: Value = serde_json::from_slice(&response.to_wire_bytes())
            .map_err(|_| invalid("capture response is invalid"))?;
        self.privacy_transformed |= redact(&mut value)?;
        self.response_json =
            Some(serde_json::to_string(&value).map_err(|_| invalid("capture encoding failed"))?);
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenReplayInputs {
    pub format_version: u64,
    pub computation_version: String,
    pub canonicalization_version: String,
    pub privacy_version: String,
    pub privacy_transformed: bool,
    pub numeric_profile: NumericProfile,
    pub provider: String,
    pub roster_complete: bool,
    pub visible_roster: Vec<FrozenMember>,
    pub numeric_inputs: Vec<FrozenScoreInput>,
    pub stages: Vec<FrozenStage>,
    pub artifact_digest: String,
}
impl FrozenReplayInputs {
    pub fn new(provider: &str) -> Self {
        Self {
            format_version: FORMAT_VERSION,
            computation_version: COMPUTATION_VERSION.into(),
            canonicalization_version: "jev-canonical-json-v1".into(),
            privacy_version: "redactor-v1".into(),
            privacy_transformed: false,
            numeric_profile: NumericProfile::current(),
            provider: provider.into(),
            roster_complete: false,
            visible_roster: Vec::new(),
            numeric_inputs: Vec::new(),
            stages: Vec::new(),
            artifact_digest: String::new(),
        }
    }
    pub fn computation_compatible(&self) -> bool {
        self.computation_version == COMPUTATION_VERSION
            && self.numeric_profile == NumericProfile::current()
    }
    pub fn exact_compatible(&self) -> bool {
        self.computation_compatible()
            && self.roster_complete
            && !self.privacy_transformed
            && self
                .stages
                .iter()
                .all(|s| !s.privacy_transformed && s.response_json.is_some())
    }
    /// Final historical output can also contain untrusted returned-model prose.
    /// Keep the transform explicit rather than exporting a secret or claiming original bytes.
    pub fn redact_history(case: &mut ReplayCase) -> Result<(), ReplayError> {
        let changed = redact(&mut case.historical_decision)?;
        if let Some(frozen) = &mut case.frozen_inputs {
            frozen.privacy_transformed |= changed;
        }
        Ok(())
    }
    pub fn phase(&self) -> Option<String> {
        let stage = self.stages.iter().find(|s| s.stage == "wide")?;
        let request = Request::from_json(stage.request_json.as_bytes()).ok()?;
        let response = request
            .decode_response(stage.response_json.as_ref()?.as_bytes())
            .ok()?;
        let Answer::Choice(answer) = response.answers.get(wide::PHASE)? else {
            return None;
        };
        answer
            .normalized_probabilities()
            .iter()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(id, _)| id.clone())
    }
    pub fn seal(case: &mut ReplayCase) -> Result<(), ReplayError> {
        if let Some(mut inputs) = case.frozen_inputs.take() {
            inputs.artifact_digest.clear();
            inputs.artifact_digest = artifact_digest(case, &inputs)?;
            case.frozen_inputs = Some(inputs);
        }
        Ok(())
    }
    pub fn validate(&self, case: &ReplayCase) -> Result<(), ReplayError> {
        if self.canonicalization_version != "jev-canonical-json-v1"
            || self.format_version != FORMAT_VERSION
            || self.privacy_version != "redactor-v1"
            || !matches!(self.provider.as_str(), "typesafe" | "cloudflare" | "local")
        {
            return Err(invalid("unsupported frozen input format"));
        }
        if self.visible_roster.len() > 10_000
            || self.numeric_inputs.len() > wide::MAX_REAL_OPTIONS
            || self.stages.len() > 2
        {
            return Err(invalid("frozen collection exceeds its bound"));
        }
        let mut unsealed = self.clone();
        unsealed.artifact_digest.clear();
        if self.artifact_digest != artifact_digest(case, &unsealed)? {
            return Err(invalid("frozen artifact digest mismatch"));
        }
        for field in [
            &self.computation_version,
            &self.numeric_profile.backend,
            &self.numeric_profile.target,
            &self.numeric_profile.build,
        ] {
            if field.is_empty() || field.len() > 512 {
                return Err(invalid("invalid computation profile field"));
            }
        }
        for hash in [
            &self.numeric_profile.dependency_digest,
            &self.numeric_profile.computation_digest,
            &self.numeric_profile.probe_digest,
        ] {
            valid_digest(hash)?;
        }
        let mut members = BTreeSet::new();
        for member in &self.visible_roster {
            valid_skill(&member.skill_id)?;
            valid_digest(&member.content_hash)?;
            if !members.insert(member.skill_id.as_str()) || member.visibility.len() > 128 {
                return Err(invalid("invalid frozen membership"));
            }
        }
        for candidate in &case.captured_request.candidate_options {
            let member = self
                .visible_roster
                .iter()
                .find(|m| m.skill_id == candidate.skill_id)
                .ok_or_else(|| invalid("candidate missing frozen membership"))?;
            if member.content_hash != candidate.content_hash
                || candidate
                    .visibility
                    .as_deref()
                    .is_some_and(|v| v != member.visibility)
            {
                return Err(invalid("frozen candidate membership mismatch"));
            }
        }
        let mut numeric = BTreeSet::new();
        for input in &self.numeric_inputs {
            if !members.contains(input.skill_id.as_str())
                || !numeric.insert(input.skill_id.as_str())
                || !input.prior_delta.is_finite()
                || !(0.0..=1.0).contains(&input.phase_match)
            {
                return Err(invalid("invalid frozen numeric input"));
            }
        }
        if !self.stages.is_empty()
            && numeric
                != case
                    .captured_request
                    .candidate_options
                    .iter()
                    .map(|c| c.skill_id.as_str())
                    .collect()
        {
            return Err(invalid("frozen numeric inputs do not cover candidate set"));
        }
        let mut stages = BTreeSet::new();
        for stage in &self.stages {
            if !stages.insert(stage.stage.as_str())
                || !matches!(stage.stage.as_str(), "wide" | "rerank")
            {
                return Err(invalid("duplicate or unknown frozen stage"));
            }
            validate_stage(stage, case, &self.provider)?;
        }
        if case.recorded_responses.wide.is_some() && !stages.contains("wide")
            || case.recorded_responses.rerank.is_some() && !stages.contains("rerank")
        {
            return Err(invalid("projected response lacks frozen stage"));
        }
        let recorded: Vec<&str> = self
            .stages
            .iter()
            .filter(|s| s.response_json.is_some())
            .map(|s| s.stage.as_str())
            .collect();
        if recorded
            != case
                .manifest
                .stages_recorded
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
        {
            return Err(invalid("frozen stage completeness mismatch"));
        }
        if stages.contains("rerank") && !stages.contains("wide") {
            return Err(invalid("rerank lacks wide input"));
        }
        Ok(())
    }
}

fn validate_stage(
    stage: &FrozenStage,
    case: &ReplayCase,
    provider: &str,
) -> Result<(), ReplayError> {
    let (question, version) = if stage.stage == "wide" {
        (wide::WHICH, wide::WIDE_POLICY_VERSION)
    } else {
        (rerank::RERANK, rerank::RERANK_POLICY_VERSION)
    };
    if stage.questions_version != version
        || stage.request_json.len() > crate::jev::codec::MAX_REQUEST_BYTES
        || stage
            .response_json
            .as_ref()
            .is_some_and(|s| s.len() > crate::jev::codec::MAX_RESPONSE_BYTES)
    {
        return Err(invalid("invalid frozen stage bounds or version"));
    }
    if stage.request_format != provider
        || stage.wire_version != "jev-provider-wire-v1"
        || stage.wire_request_json.len() > crate::jev::codec::MAX_REQUEST_BYTES
    {
        return Err(invalid("invalid frozen wire version or bounds"));
    }
    super::parse_bounded_json(
        stage.wire_request_json.as_bytes(),
        crate::output::MAX_OUTPUT_DEPTH,
    )?;
    let request = Request::from_json(stage.request_json.as_bytes())
        .map_err(|_| invalid("invalid frozen request"))?;
    if !stage.privacy_transformed
        && request
            .to_wire_json(provider_format(provider)?)
            .map_err(|_| invalid("invalid frozen wire encoding"))?
            != stage.wire_request_json.as_bytes()
    {
        return Err(invalid("frozen logical and wire requests differ"));
    }
    if Some(request.model()) != case.manifest.model.as_deref() {
        return Err(invalid("frozen model mismatch"));
    }
    let criteria = match request.questions().get(question) {
        Some(Question::Choice { criteria, .. }) => criteria,
        _ => return Err(invalid("missing frozen choice question")),
    };
    if stage.stage == "wide" {
        let expected: BTreeSet<&str> = [
            wide::WHICH,
            wide::GATE_SPECIALIZED_METHOD,
            wide::GATE_MATERIAL_HELP,
            wide::GATE_CONTEXT_SUFFICES,
            wide::PHASE,
        ]
        .into_iter()
        .collect();
        if expected != request.questions().keys().map(String::as_str).collect() {
            return Err(invalid("frozen wide question set mismatch"));
        }
    } else {
        let expected: BTreeSet<String> = std::iter::once(rerank::RERANK.to_owned())
            .chain(
                stage
                    .options
                    .iter()
                    .map(|o| format!("{}{}", rerank::FIT_PREFIX, o.option_id)),
            )
            .collect();
        if expected != request.questions().keys().cloned().collect() {
            return Err(invalid("frozen rerank question set mismatch"));
        }
    }
    if stage.options.len() > wide::MAX_REAL_OPTIONS
        || stage
            .options
            .windows(2)
            .any(|pair| pair[0].option_id >= pair[1].option_id)
    {
        return Err(invalid("invalid frozen option ordering or count"));
    }
    let mut options = BTreeSet::new();
    let mut skills = BTreeSet::new();
    for option in &stage.options {
        valid_skill(&option.skill_id)?;
        valid_digest(&option.content_hash)?;
        crate::identity::OptionId::new(&option.option_id)
            .map_err(|_| invalid("invalid frozen option identity"))?;
        if option.option_id == wide::NONE_OPTION
            || !options.insert(option.option_id.as_str())
            || !skills.insert(option.skill_id.as_str())
        {
            return Err(invalid("duplicate frozen option or skill definition"));
        }
        let candidate = case
            .captured_request
            .candidate_options
            .iter()
            .find(|c| c.skill_id == option.skill_id)
            .ok_or_else(|| invalid("foreign frozen option"))?;
        if candidate.content_hash != option.content_hash {
            return Err(invalid("frozen option content mismatch"));
        }
    }
    options.insert(wide::NONE_OPTION);
    if options != criteria.keys().map(String::as_str).collect() {
        return Err(invalid("frozen choice option mismatch"));
    }
    if stage.stage == "wide"
        && skills
            != case
                .captured_request
                .candidate_options
                .iter()
                .map(|c| c.skill_id.as_str())
                .collect()
    {
        return Err(invalid("incomplete frozen wide map"));
    }
    let Some(bytes) = &stage.response_json else {
        if (stage.stage == "wide" && case.recorded_responses.wide.is_some())
            || (stage.stage == "rerank" && case.recorded_responses.rerank.is_some())
        {
            return Err(invalid("projected answer lacks frozen response"));
        }
        return Ok(());
    };
    let response = request
        .decode_response(bytes.as_bytes())
        .map_err(|_| invalid("invalid frozen response"))?;
    let choice = match response.answers.get(question) {
        Some(Answer::Choice(c)) => c,
        _ => return Err(invalid("missing frozen choice answer")),
    };
    let resolve = |id: &str| -> Result<&str, ReplayError> {
        if id == wide::NONE_OPTION {
            Ok(wide::NONE_OPTION)
        } else {
            stage
                .options
                .iter()
                .find(|o| o.option_id == id)
                .map(|o| o.skill_id.as_str())
                .ok_or_else(|| invalid("foreign frozen answer"))
        }
    };
    let normalized = choice.normalized_probabilities();
    let (distribution, selected, confidence) = if stage.stage == "wide" {
        let projected = case
            .recorded_responses
            .wide
            .as_ref()
            .ok_or_else(|| invalid("missing projected wide response"))?;
        let answer = |id: &str| match response.answers.get(id) {
            Some(Answer::Noul(v)) => Some(*v),
            _ => None,
        };
        let gate = wide::needs_skill(
            answer(wide::GATE_SPECIALIZED_METHOD).ok_or_else(|| invalid("missing frozen gate"))?,
            answer(wide::GATE_MATERIAL_HELP).ok_or_else(|| invalid("missing frozen gate"))?,
            answer(wide::GATE_CONTEXT_SUFFICES).ok_or_else(|| invalid("missing frozen gate"))?,
        );
        if projected.gate_score != Some(gate) {
            return Err(invalid("frozen gate projection mismatch"));
        }
        (&projected.distribution, projected.choice.as_str(), None)
    } else {
        let projected = case
            .recorded_responses
            .rerank
            .as_ref()
            .ok_or_else(|| invalid("missing projected rerank response"))?;
        for option in &stage.options {
            let Some(Answer::Noul(value)) =
                response
                    .answers
                    .get(&format!("{}{}", rerank::FIT_PREFIX, option.option_id))
            else {
                return Err(invalid("missing frozen fit answer"));
            };
            if projected
                .fits
                .iter()
                .find(|f| f.skill_id == option.skill_id)
                .map(|f| f.fit)
                != Some(*value)
            {
                return Err(invalid("frozen fit projection mismatch"));
            }
        }
        (
            &projected.distribution,
            projected.choice.as_str(),
            projected.stated_confidence,
        )
    };
    if resolve(choice.choice())? != selected
        || confidence.is_some_and(|c| c != choice.confidence())
        || normalized.len() != distribution.len()
    {
        return Err(invalid("frozen choice projection mismatch"));
    }
    for (id, probability) in normalized {
        if distribution
            .iter()
            .find(|d| d.option_id == resolve(&id).unwrap_or(""))
            .map(|d| d.probability)
            != Some(probability)
        {
            return Err(invalid("frozen probability projection mismatch"));
        }
    }
    Ok(())
}

fn redact(value: &mut Value) -> Result<bool, ReplayError> {
    let mut changed = false;
    match value {
        Value::String(text) => {
            let field = Redactor::default()
                .redact_field(text)
                .map_err(|_| invalid("capture prose exceeds redaction bounds"))?;
            changed = field.redaction_count() > 0;
            *text = field.into_string();
        }
        Value::Array(items) => {
            for item in items {
                changed |= redact(item)?;
            }
        }
        Value::Object(items) => {
            for item in items.values_mut() {
                changed |= redact(item)?;
            }
        }
        _ => {}
    }
    Ok(changed)
}
fn valid_skill(id: &str) -> Result<(), ReplayError> {
    if id == wide::NONE_OPTION {
        return Err(invalid("none sentinel cannot define a frozen skill"));
    }
    SkillId::new(id)
        .map(|_| ())
        .map_err(|_| invalid("invalid frozen skill identity"))
}
fn valid_digest(hash: &str) -> Result<(), ReplayError> {
    ContentHash::parse(hash)
        .map(|_| ())
        .map_err(|_| invalid("invalid frozen content digest"))
}
fn provider_format(provider: &str) -> Result<RequestFormat, ReplayError> {
    match provider {
        "typesafe" => Ok(RequestFormat::TypeSafe),
        "cloudflare" => Ok(RequestFormat::Cloudflare),
        _ => Err(invalid("unsupported frozen provider format")),
    }
}
fn invalid(message: &str) -> ReplayError {
    ReplayError::InvalidField(message.into())
}
fn digest(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}
fn artifact_digest(case: &ReplayCase, inputs: &FrozenReplayInputs) -> Result<String, ReplayError> {
    struct HashWriter(blake3::Hasher);
    impl Write for HashWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = HashWriter(blake3::Hasher::new());
    serde_json::to_writer(
        &mut writer,
        &(
            case.schema_version,
            &case.case_id,
            case.created_at_unix_ms,
            &case.manifest,
            &case.captured_request,
            &case.recorded_responses,
            &case.local_evidence,
            &case.historical_decision,
            inputs,
        ),
    )
    .map_err(|_| invalid("frozen digest encoding failed"))?;
    Ok(writer.0.finalize().to_hex().to_string())
}
