# Context Disclosure Profiles Contract (P3)

This document formalizes the contract for standard and minimal context disclosure profiles, trust-layer resolution, and tool restriction policy (`p3_disclosure_profiles`, bead `sr-roadmap-l1i.4.11`).

## 1. Scope and Mission

SkillRanker constructs bounded, privacy-sanitized provider context payloads from live agent session transcripts to rank skill recommendations. To give users precise control over data disclosure, SkillRanker provides two standardized context disclosure profiles:

- **Standard Profile (`ContextProfile::Standard`)**: Preserves the default request-first context architecture. Includes the latest user request, indispensable task anchor, explicit constraints, loaded skill references, recent message history within budget, structured tool summaries (with redacted head/tail excerpts), and safe repository-relative dirty paths.
- **Minimal Profile (`ContextProfile::Minimal`)**: Drastically reduces disclosed surface area. Sends only the current request, indispensable bounded task anchor, explicit constraints, and per-stage candidate material required for ranking, while **completely omitting** optional message history, tool bodies/arguments/results, and repository dirty paths.

This boundary guarantees that:
1. `--context-profile standard|minimal` governs remote payload construction without altering local observation semantics.
2. `--no-tools` acts as an orthogonal restriction that further strips all tool arguments and results under both profiles.
3. Essential missing evidence (omitted essential attachments or essential tool results) produces `unavailable / unsupported-context` rather than pretending a reduced payload is adequate.
4. Trust layers enforce strict **non-widening**: project configuration cannot widen a trusted minimal profile established by user configuration or CLI.
5. Local load observations (`extract_load_observations`) remain strictly invariant to the active disclosure profile.
6. Actual disclosed byte volume can be audited (`payload.disclosed_bytes()` / `count_disclosed_bytes`) without claiming quality equivalence.

## 2. Invariants

### Invariant 1: Field Disclosure Policy
The fields disclosed under each profile and tool flag combination are strictly defined:

| Field | Standard (`no-tools=false`) | Standard (`no-tools=true`) | Minimal (`no-tools=false`) | Minimal (`no-tools=true`) |
|---|:---:|:---:|:---:|:---:|
| `latest_user_request` | Disclosed | Disclosed | Disclosed | Disclosed |
| `task_anchor` | Disclosed | Disclosed | Disclosed | Disclosed |
| `explicit_constraints` | Disclosed | Disclosed | Disclosed | Disclosed |
| `candidate_material` | Disclosed | Disclosed | Disclosed | Disclosed |
| `recent_messages` | Disclosed (budgeted) | Disclosed (no tools) | **Omitted** (empty) | **Omitted** (empty) |
| `tool_bodies` | Disclosed (summarized) | **Omitted** | **Omitted** | **Omitted** |
| `dirty_paths` | Disclosed | Disclosed | **Omitted** (empty) | **Omitted** (empty) |

### Invariant 2: Non-Widening Trust Layer Hierarchy
Profile configuration follows a strict authority hierarchy:
1. **Built-in Default**: `ContextProfile::Standard`.
2. **User Configuration**: May set `Standard` or `Minimal`.
3. **Project Configuration (`restrict-only`)**:
   - If user setting is `Minimal`, a project setting of `Standard` is rejected with `ProfileTrustError::ProjectCannotWidenProfile`.
   - If user setting is `Standard`, a project setting of `Minimal` narrows the profile to `Minimal`.
4. **Explicit CLI Flag**: The interactive command-line argument (`--context-profile`) is authoritative and reflects explicit human operator intent, overriding configuration layers.

### Invariant 3: Essential Evidence Omission Semantics
When context rendering omits attachments or tool results that are essential to understanding the user request:
- If `current_request.essential_attachment_missing` is true, or if an essential tool result is omitted (via `Minimal` profile or `--no-tools` when the request depends on prior tool execution), context quality is classified as `ContextQuality::Insufficient`.
- If `fail_on_unsupported_context` is enabled, rendering returns `Err(RenderContextError::UnsupportedContext(...))`.
- SkillRanker fails closed with decision `Unavailable` (`unavailable / unsupported-context`), rather than pretending a partial payload suffices.

### Invariant 4: Independence of Local Load Observations
- Local skill load observations (`extract_load_observations`) are derived directly from normalized transcript events and the local skill evidence resolver.
- Omission of message history, tool bodies, or dirty paths in the provider payload does **not** alter, suppress, or modify local load observations.
- Local learning, feedback, and telemetry remain grounded in actual execution evidence, regardless of remote disclosure restrictions.

### Invariant 5: Auditability and Disclosed Byte Volume
- The active `context_profile` is recorded in `RenderedContextPayload` for audit and request provenance.
- The total byte volume of the serialized payload is calculable via `payload.disclosed_bytes()`.
- Disclosed bytes under `Minimal` profile are strictly less than under `Standard` profile for identical session history.
- Reduced disclosure volume does not claim quality equivalence.

## 3. Data Model and API

```rust
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextProfile {
    Minimal,
    #[default]
    Standard,
}

pub struct ProfileDisclosedFields {
    pub current_request: bool,
    pub task_anchor: bool,
    pub explicit_constraints: bool,
    pub candidate_material: bool,
    pub recent_messages: bool,
    pub tool_bodies: bool,
    pub dirty_paths: bool,
}

pub enum ProfileTrustError {
    ProjectCannotWidenProfile {
        user: ContextProfile,
        project: ContextProfile,
    },
    InvalidProfile(String),
}

pub fn validate_project_profile(
    user: ContextProfile,
    project: ContextProfile,
) -> Result<ContextProfile, ProfileTrustError>;

pub fn resolve_context_profile(
    user: Option<ContextProfile>,
    project: Option<ContextProfile>,
    cli: Option<ContextProfile>,
) -> Result<ContextProfile, ProfileTrustError>;

pub fn count_disclosed_bytes(payload: &RenderedContextPayload) -> usize;
```
