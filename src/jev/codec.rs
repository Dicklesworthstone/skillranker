//! Pure codecs for the documented TypeSafe `/v1/systemone` wire protocol.
//! No transport, redaction, ranking policy, or permission is implied by decoding.
//! Callers must redact every field before encoding and authorize any transmission.

use crate::output::JsonSeed;
use serde::de::DeserializeSeed;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Write};

pub const MAX_REQUEST_BYTES: usize = 96 * crate::limits::KIB;
pub const MAX_RESPONSE_BYTES: usize = 2 * crate::limits::MIB;
pub const MAX_CHOICE_OPTIONS: usize = 255;
pub const SUM_TOLERANCE: f64 = 1e-4;

/// Errors deliberately contain no input, provider body, keys, or parser details.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodecError {
    TooLarge,
    InvalidJson,
    InvalidRequest,
    DuplicateId,
    InvalidAnswer,
    QuestionMismatch,
    OptionMismatch,
    InvalidProbability,
    InvalidDistribution,
    InvalidChoice,
}
impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TooLarge => "Jev document exceeds byte limit",
            Self::InvalidJson => "invalid bounded Jev JSON",
            Self::InvalidRequest => "invalid Jev request",
            Self::DuplicateId => "duplicate Jev definition",
            Self::InvalidAnswer => "invalid Jev answer or usage",
            Self::QuestionMismatch => "Jev answer questions do not match request",
            Self::OptionMismatch => "Jev answer options do not match request",
            Self::InvalidProbability => "invalid Jev probability",
            Self::InvalidDistribution => "invalid Jev distribution",
            Self::InvalidChoice => "Jev choice is not a maximum-probability option",
        })
    }
}
impl std::error::Error for CodecError {}

/// String descriptions are the conservative common documented Choice shape.
/// `stuck` is an ordinary optional Noul, not an implicit extra question.
/// Score is intentionally unavailable until the later experimental feature gate.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Question {
    Choice {
        instructions: Value,
        criteria: BTreeMap<String, String>,
    },
    Noul {
        instructions: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        criteria: Option<NoulCriteria>,
    },
}
#[derive(Clone, Serialize, Deserialize)]
pub struct NoulCriteria {
    #[serde(rename = "true", skip_serializing_if = "Option::is_none")]
    pub yes: Option<String>,
    #[serde(rename = "false", skip_serializing_if = "Option::is_none")]
    pub no: Option<String>,
}

impl Question {
    /// Use an iterator rather than a pre-collected map to detect duplicate IDs.
    pub fn choice(
        instructions: Value,
        options: impl IntoIterator<Item = (String, String)>,
    ) -> Result<Self, CodecError> {
        let mut criteria = BTreeMap::new();
        for (id, description) in options {
            if criteria.insert(id, description).is_some() {
                return Err(CodecError::DuplicateId);
            }
            if criteria.len() > MAX_CHOICE_OPTIONS {
                return Err(CodecError::InvalidRequest);
            }
        }
        let question = Self::Choice { instructions, criteria };
        question.validate()?;
        Ok(question)
    }

    fn validate(&self) -> Result<(), CodecError> {
        let instructions = match self {
            Self::Choice { instructions, criteria } => {
                if criteria.is_empty() || criteria.len() > MAX_CHOICE_OPTIONS
                    || criteria.keys().any(|key| !valid_id(key)) {
                    return Err(CodecError::InvalidRequest);
                }
                instructions
            }
            Self::Noul { instructions, .. } => instructions,
        };
        if !description(instructions) {
            return Err(CodecError::InvalidRequest);
        }
        Ok(())
    }
}

fn description(value: &Value) -> bool {
    matches!(value, Value::String(_) | Value::Object(_) | Value::Array(_))
}
fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= crate::identity::MAX_ID_BYTES
        && !value.chars().any(char::is_control)
}

/// Immutable expected question/option map, retained locally to bind answers.
#[derive(Clone, Serialize, Deserialize)]
pub struct Request {
    model: String,
    state: Value,
    questions: BTreeMap<String, Question>,
}
impl Request {
    pub fn new(
        model: String,
        state: Value,
        questions: impl IntoIterator<Item = (String, Question)>,
    ) -> Result<Self, CodecError> {
        let mut map = BTreeMap::new();
        for (id, question) in questions {
            if map.insert(id, question).is_some() {
                return Err(CodecError::DuplicateId);
            }
        }
        let request = Self { model, state, questions: map };
        request.to_json()?;
        Ok(request)
    }

    /// For recorded/preview requests; duplicates are rejected before deserialization.
    pub fn from_json(bytes: &[u8]) -> Result<Self, CodecError> {
        let request: Self = serde_json::from_value(parse(bytes, MAX_REQUEST_BYTES)?)
            .map_err(|_| CodecError::InvalidRequest)?;
        request.to_json()?;
        Ok(request)
    }

    pub fn model(&self) -> &str { &self.model }
    pub fn questions(&self) -> &BTreeMap<String, Question> { &self.questions }

    pub fn to_json(&self) -> Result<Vec<u8>, CodecError> {
        if !valid_id(&self.model) || !description(&self.state) || self.questions.is_empty()
            || self.questions.keys().any(|id| !valid_id(id)) {
            return Err(CodecError::InvalidRequest);
        }
        for question in self.questions.values() { question.validate()?; }
        let mut buffer = BoundedBuffer(Vec::new());
        serde_json::to_writer(&mut buffer, self).map_err(|_| CodecError::TooLarge)?;
        // Reuse the same depth/duplicate boundary for locally constructed Values.
        parse(&buffer.0, MAX_REQUEST_BYTES)?;
        Ok(buffer.0)
    }

    /// Input must already be decoded/decompressed; the cap covers these bytes.
    /// Retain requested alias separately from the returned identifier.
    pub fn decode_response(&self, bytes: &[u8]) -> Result<Response, CodecError> {
        let wire: WireResponse = serde_json::from_value(parse(bytes, MAX_RESPONSE_BYTES)?)
            .map_err(|_| CodecError::InvalidAnswer)?;
        if !valid_id(&wire.model) { return Err(CodecError::InvalidAnswer); }
        if !self.questions.keys().eq(wire.answers.keys()) {
            return Err(CodecError::QuestionMismatch);
        }
        let mut answers = BTreeMap::new();
        for (id, answer) in wire.answers {
            let validated = match (&self.questions[&id], answer) {
                (Question::Noul { .. }, WireAnswer::Noul { noul }) => {
                    probability(noul)?;
                    Answer::Noul(noul)
                }
                (Question::Choice { criteria, .. }, WireAnswer::Choice {
                    choice, probabilities, confidence,
                }) => {
                    if !criteria.keys().eq(probabilities.keys()) {
                        return Err(CodecError::OptionMismatch);
                    }
                    probability(confidence)?;
                    let mut sum = 0.0;
                    let mut maximum = 0.0_f64;
                    for &value in probabilities.values() {
                        probability(value)?;
                        sum += value;
                        maximum = maximum.max(value);
                    }
                    if sum <= 0.0 || (sum - 1.0).abs() > SUM_TOLERANCE {
                        return Err(CodecError::InvalidDistribution);
                    }
                    if probabilities.get(&choice).copied() != Some(maximum) {
                        return Err(CodecError::InvalidChoice);
                    }
                    Answer::Choice(ChoiceAnswer {
                        choice, raw_probabilities: probabilities, raw_sum: sum, confidence,
                    })
                }
                _ => return Err(CodecError::QuestionMismatch),
            };
            answers.insert(id, validated);
        }
        Ok(Response {
            requested_model: self.model.clone(), returned_model: wire.model,
            answers, usage: wire.usage,
        })
    }
}

// No Debug derives on raw fields: even model IDs and question keys may be private.
#[derive(Deserialize)]
struct WireResponse {
    model: String,
    answers: BTreeMap<String, WireAnswer>,
    usage: Usage,
}
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum WireAnswer {
    Noul { noul: f64 },
    Choice { choice: String, probabilities: BTreeMap<String, f64>, confidence: f64 },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

pub struct Response {
    pub requested_model: String,
    pub returned_model: String,
    pub answers: BTreeMap<String, Answer>,
    pub usage: Usage,
}
pub enum Answer {
    Noul(f64),
    Choice(ChoiceAnswer),
}
/// Raw evidence remains distinct from rounding-drift normalization and scoring.
pub struct ChoiceAnswer {
    choice: String,
    raw_probabilities: BTreeMap<String, f64>,
    raw_sum: f64,
    confidence: f64,
}
impl ChoiceAnswer {
    pub fn choice(&self) -> &str { &self.choice }
    pub fn raw_probabilities(&self) -> &BTreeMap<String, f64> { &self.raw_probabilities }
    pub fn normalized_probability(&self, option: &str) -> Option<f64> {
        self.raw_probabilities.get(option).map(|value| value / self.raw_sum)
    }
    pub fn confidence(&self) -> f64 { self.confidence }
}

fn probability(value: f64) -> Result<(), CodecError> {
    if value.is_finite() && (0.0..=1.0).contains(&value) { Ok(()) }
    else { Err(CodecError::InvalidProbability) }
}
fn parse(bytes: &[u8], maximum: usize) -> Result<Value, CodecError> {
    if bytes.len() > maximum { return Err(CodecError::TooLarge); }
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    let value = JsonSeed(0).deserialize(&mut decoder).map_err(|_| CodecError::InvalidJson)?;
    decoder.end().map_err(|_| CodecError::InvalidJson)?;
    Ok(value)
}
struct BoundedBuffer(Vec<u8>);
impl Write for BoundedBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_REQUEST_BYTES.saturating_sub(self.0.len()) {
            return Err(io::Error::other("Jev request byte limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}
