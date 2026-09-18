//! Two-stage ranking pipeline and publication revalidation.
//!
//! Orchestrates context capture, roster discovery, explicit resolution,
//! Quill prefiltering, response cache, wide gate, detailed rerank, local
//! eligibility/scoring, and final boundary revalidation against changed
//! user controls and modified roster dependencies.

use asupersync::Cx;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::cache::{
    CacheKey, CacheLookupQuery, CacheLookupResult, CacheNamespace, CandidateDigest,
    MemoryResponseCache, RequestFingerprintInput, RequestStage, compute_request_fingerprint,
};
use crate::cli::ConfigFiles;
use crate::config::{
    ConfigSources, PolicyBoundary, PolicyReceipt, PublicationKind, ResolvedConfig, Revalidation,
};
use crate::context::anchor::resolve_task_anchor;
use crate::context::branch::{LoadedSkillRecord, resolve_active_branch};
use crate::context::jsonl::{CursorKind, snapshot_jsonl};
use crate::context::render::{RenderContextOptions, render_context_and_receipt};
use crate::context::source::{SelectionOutcome, SourceOptions, SourcePolicy, SourceTarget};
use crate::context::{CurrentRequest, NormalizedContext, PrivateText, parse_normalized_context};
use crate::effects::EffectGate;
use crate::eligibility::{Eligible, LoadedState, Verdict, admit, after_rerank};
use crate::identity::{EventId, HarnessId, SessionId, SkillId, WorkspaceId};
use crate::jev::client::{JevClient, TransportError, TransportErrorKind};
use crate::jev::codec::{Request, Response};
use crate::jev::endpoint::EndpointConfig;
use crate::jev::rerank::{RerankOutcome, RerankRequest};
use crate::jev::wide::{Sizes, WideDecision, WideOutcome, WideRequest};
use crate::jev::{OriginScopedCredential, rerank, wide};
use crate::output::{ErrorKind, OutputDocument, SCHEMA_VERSION};
use crate::privacy::redaction::Redactor;
use crate::privacy::{ContextProfile, NetworkConsent};
use crate::roster::discovery::claude_code_plan;
use crate::roster::explicit::{
    ExplicitResolutionRequest, ExplicitResolutionResult, ResolvedExplicitSkill,
    resolve_explicit_requirements,
};
use crate::roster::import::import_authorized;
use crate::roster::resolution::{AdvisorySkill, ResolvedRoster, resolve_claude_plan};
use crate::roster::retrieval::{QueryInput, RetrievalBudget, RetrievalError, retrieve};
use crate::roster::revalidation::{RevalidationError, capture, revalidate_claude};
use crate::roster::{LoadTarget, Visibility};
use crate::runtime::{EntryClock, ProcessInvocation, admit_publication};
use crate::scoring::{Input as ScoringInput, Ranking, Weights, rank};

/// Failure tuple compatible with CLI error formatting: `(exit_code, kind, message)`.
pub type PipelineFailure = (u8, &'static str, String);

fn failure(code: u8, kind: &'static str, message: impl Into<String>) -> PipelineFailure {
    (code, kind, message.into())
}

/// Abstract transport interface for Jev requests, enabling real TLS sockets
/// or controlled test fixture injection.
pub trait JevTransport: Send + Sync {
    fn send<'a>(
        &'a self,
        request: &'a Request,
        credential: Option<&'a OriginScopedCredential>,
        consent: NetworkConsent,
        cx: &'a Cx,
        clock: &'a EntryClock,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Response, TransportError>> + Send + 'a>,
    >;
}

impl JevTransport for JevClient {
    fn send<'a>(
        &'a self,
        request: &'a Request,
        credential: Option<&'a OriginScopedCredential>,
        consent: NetworkConsent,
        cx: &'a Cx,
        clock: &'a EntryClock,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Response, TransportError>> + Send + 'a>,
    > {
        Box::pin(self.send(request, credential, consent, cx, clock))
    }
}

/// Parameters for running the ranking pipeline.
#[derive(Clone, Debug)]
pub struct RankArgs {
    pub workspace: PathBuf,
    pub user_config_root: Option<PathBuf>,
    pub sources: ConfigSources,
    pub gate: EffectGate,
    pub source_options: SourceOptions,
    pub require_skills: Vec<SkillId>,
    pub roster_file: Option<PathBuf>,
    pub explain: bool,
    pub why_not: Option<SkillId>,
    pub output_json: bool,
    pub output_table: bool,
    pub dry_run: bool,
}

/// Pipeline execution state tracking provider attempts, requests, tokens and cache hits.
#[derive(Clone, Debug, Default)]
pub struct ExecutionMetrics {
    pub requests: u64,
    pub http_attempts: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub unknown_usage_attempts: u64,
    pub cache_hit: bool,
    pub wide_hit: bool,
    pub rerank_hit: bool,
    pub cache_age_ms: Option<u64>,
}

/// Assembles and executes the two-stage rank pipeline.
pub async fn execute_pipeline(
    clock: &EntryClock,
    cx: &Cx,
    args: RankArgs,
    transport: Option<&dyn JevTransport>,
) -> Result<OutputDocument, PipelineFailure> {
    clock
        .admit_new_work()
        .map_err(|_| failure(6, "timeout", "Invocation deadline exceeded before ranking start"))?;

    // 1. Initial configuration loading and policy receipt capture
    let config_files = ConfigFiles::new(args.workspace.clone(), args.user_config_root.clone());
    let resolved_config = config_files.load(clock, args.sources)?;
    let mut current_receipt = resolved_config.receipt(args.gate.policy());

    let effective = resolved_config.effective();
    let top = effective.top() as usize;
    let shortlist = effective.shortlist() as usize;
    let gate_threshold = effective.gate();
    let fits_threshold = effective.fits();

    // Validate size invariants: 1 <= top <= shortlist <= 32
    let sizes = Sizes::new(top, shortlist).map_err(|e| {
        failure(
            2,
            "invalid-configuration",
            format!("Invalid ranking sizes: {e:?}"),
        )
    })?;

    // 2. Select and ingest context source
    let source_policy = SourcePolicy {
        offline: args.gate.policy().flags().offline,
        dry_run: args.dry_run,
        local_only: args.gate.policy().flags().offline,
        allow_network: args.gate.policy().flags().allow_network,
    };
    let workspace_id = WorkspaceId::new(args.workspace.to_string_lossy().as_ref()).map_err(|_| {
        failure(
            2,
            "invalid-configuration",
            "Invalid workspace root path",
        )
    })?;

    let selection_outcome = args.source_options.clone().resolve(
        workspace_id.clone(),
        source_policy,
        false, // non-interactive by default
        |_, _| {
            // Inventory discovery fallback if needed
            Ok(crate::context::source::SessionInventory::default())
        },
    ).map_err(|err| {
        failure(err.kind().exit_code() as u8, err.kind().as_str(), err.to_string())
    })?;

    let source_selection = match selection_outcome {
        SelectionOutcome::Selected(sel) => sel,
        SelectionOutcome::NeedsChoice(_) => {
            return Err(failure(
                2,
                "ambiguous-session",
                "Multiple sessions available; selection required",
            ));
        }
    };

    // Ingest normalized context or native transcript
    let normalized_context = match source_selection.target() {
        SourceTarget::NormalizedFile(path) => {
            let bytes = std::fs::read(path.as_path()).map_err(|e| {
                failure(7, "malformed-input", format!("Failed to read context file: {e}"))
            })?;
            parse_normalized_context(&bytes).map_err(|e| {
                failure(7, "malformed-input", format!("Invalid normalized context: {e}"))
            })?
        }
        SourceTarget::NormalizedStdin => {
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut std::io::stdin().lock(), &mut bytes).map_err(|e| {
                failure(7, "malformed-input", format!("Failed to read context from stdin: {e}"))
            })?;
            parse_normalized_context(&bytes).map_err(|e| {
                failure(7, "malformed-input", format!("Invalid normalized context: {e}"))
            })?
        }
        SourceTarget::ClaudeTranscript(path) => {
            // Snapshot JSONL transcript
            let invocation = ProcessInvocation::from_clock(*clock).map_err(|_| {
                failure(6, "timeout", "Deadline exceeded creating runtime")
            })?;
            let req_cx = invocation.request_cx().map_err(|_| {
                failure(6, "timeout", "Deadline exceeded creating context")
            })?;
            let snapshot = snapshot_jsonl(
                &invocation,
                &req_cx,
                path.as_path(),
                None,
                CursorKind::Ranking,
            ).map_err(|e| {
                failure(7, "malformed-input", format!("Transcript snapshot failed: {e}"))
            })?;

            let events = snapshot.events;
            let current_req_text = events
                .iter()
                .rev()
                .find(|e| e.role == crate::context::Role::User)
                .map(|e| e.text.clone())
                .unwrap_or_else(|| PrivateText::new(""));

            NormalizedContext {
                schema_version: 1,
                harness: HarnessId::new("claude_code").unwrap(),
                producer_id: None,
                workspace_root: PrivateText::new(args.workspace.to_string_lossy()),
                session_id: Some(SessionId::new("session-0").unwrap()),
                agent_id: None,
                branch_id: None,
                context_epoch: None,
                current_request: CurrentRequest {
                    event_id: Some(EventId::new("req-0").unwrap()),
                    text: current_req_text,
                    attachments_omitted: false,
                    essential_attachment_missing: false,
                },
                events,
                explicit_skill_references: Vec::new(),
                supplied_loads: Vec::new(),
            }
        }
        SourceTarget::ClaudeHookStdin => {
            return Err(failure(
                2,
                "unsupported-source-mode",
                "Use sr hook claude for hook protocol stdin mode",
            ));
        }
        SourceTarget::CassSession(_) => {
            return Err(failure(
                7,
                "unsupported-source-mode",
                "Cass session mode unavailable in this execution lane",
            ));
        }
    };

    // 3. Resolve active branch & task anchor
    let branch_target = crate::context::branch::BranchResolutionTarget {
        target_event_id: normalized_context.current_request.event_id.clone(),
        target_branch_id: normalized_context.branch_id.clone(),
        target_agent_id: normalized_context.agent_id.clone(),
    };
    let resolved_branch = resolve_active_branch(&normalized_context.events, &branch_target);
    let active_branch = resolved_branch.active_branch();

    let redactor = Redactor::default();
    let anchor_res = resolve_task_anchor(&normalized_context, None, &redactor);
    let task_anchor_text = match &anchor_res {
        crate::context::anchor::AnchorResolution::Established(a) => a.text.as_str(),
        _ => "",
    };

    // Combine explicit skill directives from CLI flags, context, and anchor
    let mut explicit_directives = args.require_skills.clone();
    for ref_id in &normalized_context.explicit_skill_references {
        if !explicit_directives.contains(ref_id) {
            explicit_directives.push(ref_id.clone());
        }
    }
    if let crate::context::anchor::AnchorResolution::Established(ref a) = anchor_res {
        for directive in &a.directives {
            if directive.kind == crate::context::anchor::AnchorDirectiveKind::Require {
                if let Ok(id) = SkillId::new(&directive.target) {
                    if !explicit_directives.contains(&id) {
                        explicit_directives.push(id);
                    }
                }
            }
        }
    }

    // 4. Discover Roster
    let visibility = Visibility::Verified {
        contract_version: "v1".into(),
    };
    let overrides = BTreeMap::new();
    let roster = if let Some(roster_path) = &args.roster_file {
        let bytes = std::fs::read(roster_path).map_err(|e| {
            failure(5, "unusable-roster", format!("Failed to read roster file: {e}"))
        })?;
        let plan = claude_code_plan(&args.workspace, args.user_config_root.as_deref(), visibility.clone())
            .map_err(|e| {
                failure(5, "unusable-roster", format!("Failed to create discovery plan: {e}"))
            })?;
        import_authorized(&bytes, &plan, &overrides, cx, clock).map_err(|e| {
            failure(5, "unusable-roster", format!("Failed to import roster: {e:?}"))
        })?
    } else {
        let plan = claude_code_plan(&args.workspace, args.user_config_root.as_deref(), visibility.clone())
            .map_err(|e| {
                failure(5, "unusable-roster", format!("Failed to create discovery plan: {e}"))
            })?;
        resolve_claude_plan(&plan, &overrides, cx, clock).map_err(|e| {
            failure(5, "unusable-roster", format!("Failed to resolve discovery plan: {e}"))
        })?
    };

    // 5. Explicit Directives Check (bypasses Jev and Quill)
    if !explicit_directives.is_empty() {
        let explicit_req = ExplicitResolutionRequest {
            cli_required_skills: explicit_directives
                .iter()
                .map(|id| id.as_str().to_string())
                .collect(),
            cli_excluded_skills: Vec::new(),
            context_skill_references: normalized_context.explicit_skill_references.clone(),
            context_excluded_skills: Vec::new(),
            user_prompt: Some(normalized_context.current_request.text.as_str().to_string()),
        };
        let explicit_result = resolve_explicit_requirements(&explicit_req, &roster)
            .map_err(|e| failure(2, "invalid-usage", format!("Explicit resolution error: {e}")))?;

        match explicit_result {
            ExplicitResolutionResult::Resolved { skills, .. } => {
                // Revalidate policy receipt before publication of explicit result
                let (refreshed, reval) = config_files.refresh(
                    clock,
                    &resolved_config,
                    &current_receipt,
                    PolicyBoundary::CliPublication(PublicationKind::Explicit),
                )?;
                if let Revalidation::Superseded(fields) = reval {
                    return Err(failure(
                        3,
                        "superseded",
                        format!("Policy changed during evaluation: {fields:?}"),
                    ));
                }
                if let Revalidation::InvalidConfiguration = reval {
                    return Err(failure(
                        2,
                        "invalid-configuration",
                        "Configuration became invalid before publication",
                    ));
                }
                let _ = refreshed;

                let doc = build_explicit_document(
                    &skills,
                    &normalized_context,
                    &roster,
                    clock.now().as_millis(),
                );
                return Ok(doc);
            }
            ExplicitResolutionResult::Unavailable { unresolved } => {
                let msg = if let Some(first) = unresolved.first() {
                    format!("Unresolved explicit skill: {}", first.target)
                } else {
                    "Unresolved explicit skill".to_string()
                };
                let unresolved_refs: Vec<crate::output::UnresolvedReference> = unresolved
                    .iter()
                    .map(|u| {
                        let reason = match u.reason {
                            crate::roster::explicit::UnresolvedReason::Missing => {
                                crate::output::UnresolvedReason::Missing
                            }
                            crate::roster::explicit::UnresolvedReason::Ambiguous => {
                                crate::output::UnresolvedReason::Ambiguous
                            }
                            crate::roster::explicit::UnresolvedReason::Forbidden
                            | crate::roster::explicit::UnresolvedReason::Shadowed
                            | crate::roster::explicit::UnresolvedReason::ConflictingDirective
                            | crate::roster::explicit::UnresolvedReason::Unverified
                            | crate::roster::explicit::UnresolvedReason::InvalidName => {
                                crate::output::UnresolvedReason::Restricted
                            }
                        };
                        crate::output::UnresolvedReference {
                            reference: u.target.clone(),
                            reason,
                        }
                    })
                    .collect();
                let doc = OutputDocument::failure_with_details(
                    ErrorKind::UnresolvedExplicit,
                    &msg,
                    "Inspect available skills in the roster or configure skill roots.",
                    false,
                )
                .with_unresolved(unresolved_refs)
                .map_err(|e| {
                    failure(5, "unresolved-explicit", format!("Contract error: {e:?}"))
                })?;
                return Ok(doc);
            }
            ExplicitResolutionResult::NoneSpecified { .. } => {}
        }
    }

    // 6. Advisory candidate admission & local policy filtering
    let loaded_records: Vec<LoadedSkillRecord> = Vec::new();
    let loaded_state = LoadedState {
        branch: active_branch,
        records: &loaded_records,
    };

    let mut excluded_skills = BTreeSet::new();
    for ex in effective.exclude_skills() {
        if let Ok(id) = SkillId::new(ex.as_str()) {
            excluded_skills.insert(id);
        }
    }
    let excluded_refs: BTreeSet<&SkillId> = excluded_skills.iter().collect();

    // Roster skills as AdvisorySkills
    let mut initial_advisory: Vec<AdvisorySkill> = Vec::new();
    for skill in roster.skills() {
        for binding in skill.bindings() {
            if matches!(binding.visibility, Visibility::Verified { .. }) {
                initial_advisory.push(AdvisorySkill {
                    record: skill.record(),
                    binding,
                });
            }
        }
    }

    if initial_advisory.is_empty() {
        let doc = OutputDocument::failure_with_details(
            ErrorKind::EmptyRoster,
            "No eligible skills found in roster",
            "Check skill directory paths or frontmatter syntax.",
            false,
        );
        return Ok(doc);
    }

    // Initial admission before Wide
    let admission = admit(&initial_advisory, &excluded_refs, loaded_state);
    if let Some(verdict) = admission.verdict {
        match verdict {
            Verdict::Abstain(reason) => {
                let doc = build_abstain_document(
                    reason.as_str(),
                    &normalized_context,
                    &roster,
                    clock.now().as_millis(),
                    None,
                );
                return Ok(doc);
            }
            Verdict::Unavailable(reason) => {
                let doc = OutputDocument::failure_with_details(
                    reason.kind(),
                    &format!("Candidate unavailable: {}", reason.as_str()),
                    "Check candidate eligibility and loaded state.",
                    false,
                );
                return Ok(doc);
            }
        }
    }

    // 7. Bounded Quill retrieval if > 254 candidates
    let candidate_skills = if admission.admitted.len() > wide::MAX_REAL_OPTIONS {
        let query_input = QueryInput {
            latest_request: normalized_context.current_request.text.as_str(),
            active_task: task_anchor_text,
            recent_errors: "",
        };
        let budget = RetrievalBudget::default();
        let selection = retrieve(&roster, &excluded_skills, query_input, budget, cx, clock)
            .await
            .map_err(|err| match err.kind {
                RetrievalError::RetrievalEmpty => failure(5, "retrieval-empty", "Quill retrieval yielded 0 matches"),
                RetrievalError::Deadline => failure(6, "timeout", "Retrieval exceeded deadline"),
                _ => failure(5, "retrieval-failure", format!("Retrieval error: {err}")),
            })?;
        selection.candidates
    } else {
        admission.admitted
    };

    if candidate_skills.is_empty() {
        let doc = build_abstain_document(
            "no-shortlist-match",
            &normalized_context,
            &roster,
            clock.now().as_millis(),
            None,
        );
        return Ok(doc);
    }

    // 8. Capture roster dependencies for final revalidation
    let mut content_scope = BTreeSet::new();
    for candidate in &candidate_skills {
        content_scope.insert(candidate.binding.id.clone());
    }
    let dependencies = capture(&roster, &content_scope, clock).map_err(|e| {
        failure(5, "unusable-roster", format!("Failed to capture dependencies: {e:?}"))
    })?;

    // 9. Render context payload for Jev
    let render_opts = RenderContextOptions {
        no_tools: effective.no_tools(),
        context_profile: ContextProfile::Standard,
        max_messages: effective.messages() as usize,
        max_total_scalars: effective.budget_chars() as usize,
        redactor: redactor.clone(),
        ..Default::default()
    };
    let (rendered_context, _receipt) = render_context_and_receipt(&normalized_context, &render_opts)
        .map_err(|e| failure(7, "oversized-input", format!("Context render error: {e:?}")))?;

    // 10. Cache check for Wide Stage
    let cache_key = CacheKey::from_bytes([42u8; 32]);
    let cache_ns = CacheNamespace::new(
        normalized_context.harness.clone(),
        current_receipt.generation(),
    )
    .with_workspace(workspace_id.clone());

    let candidate_ids: Vec<SkillId> = candidate_skills.iter().map(|s| s.binding.id.clone()).collect();
    let candidate_digests: Vec<CandidateDigest> = candidate_skills
        .iter()
        .map(|s| CandidateDigest {
            skill_id: s.binding.id.clone(),
            content_hash: s.record.source_content.clone(),
            excerpt_hash: None,
        })
        .collect();

    let wide_req_fp = compute_request_fingerprint(
        &cache_key,
        &cache_ns,
        &RequestFingerprintInput {
            stage: RequestStage::Wide,
            canonical_redacted_state: &rendered_context.to_json_bytes().unwrap_or_default(),
            candidates: &candidate_digests,
            questions_digest: [0u8; 32],
            endpoint_url: "https://api.typesafe.ai/v1/systemone",
            model: effective.model().as_str(),
            prompt_version: wide::WIDE_POLICY_VERSION,
            adapter_version: "v1",
            privacy_policy_version: "v1",
            excerpt_strategy: "default",
        },
    );

    let memory_cache = MemoryResponseCache::new();
    let mut metrics = ExecutionMetrics::default();

    // 11. Stage 1 (Wide) Call or Cache Hit
    let wide_builder = wide::build(
        &roster,
        &candidate_ids,
        &rendered_context,
        effective.model().as_str(),
        false, // include_stuck
    ).map_err(|e| failure(e.kind().exit_code() as u8, e.kind().as_str(), format!("Wide build failed: {e:?}")))?;

    // Check dry-run
    if args.dry_run {
        let dry_run_json = json!({
            "dry_run": true,
            "stage": "wide",
            "request_bytes": wide_builder.bytes().len(),
            "model": effective.model().as_str(),
            "candidates": candidate_ids.len(),
            "trimming": {
                "dropped_messages": wide_builder.trimming().dropped_messages,
                "description_cap": wide_builder.trimming().description_cap,
                "omitted_description_scalars": wide_builder.trimming().omitted_description_scalars,
                "redactions": wide_builder.trimming().redactions,
            }
        });
        let doc = build_abstain_document(
            "dry-run-preview",
            &normalized_context,
            &roster,
            clock.now().as_millis(),
            Some(dry_run_json),
        );
        return Ok(doc);
    }

    let wide_response = if !args.gate.policy().flags().no_cache {
        let lookup_q = CacheLookupQuery {
            key: &cache_key,
            namespace: &cache_ns,
            stage: RequestStage::Wide,
            fingerprint: &wide_req_fp,
            now_unix_ms: clock.now().as_millis(),
            active_model: effective.model().as_str(),
            active_revision: None,
        };
        match memory_cache.get(&lookup_q) {
            Ok(CacheLookupResult::Hit { entry, .. }) => {
                metrics.wide_hit = true;
                metrics.cache_hit = true;
                wide_builder.request().decode_response(&entry.response_bytes).map_err(|e| {
                    failure(10, "invalid-provider-response", format!("Corrupt cached wide response: {e:?}"))
                })?
            }
            _ => {
                // Cache miss: execute provider attempt
                execute_provider_wide(
                    clock,
                    cx,
                    &config_files,
                    &resolved_config,
                    &mut current_receipt,
                    &args.gate,
                    &wide_builder,
                    transport,
                    &mut metrics,
                ).await?
            }
        }
    } else {
        execute_provider_wide(
            clock,
            cx,
            &config_files,
            &resolved_config,
            &mut current_receipt,
            &args.gate,
            &wide_builder,
            transport,
            &mut metrics,
        ).await?
    };

    // Evaluate Wide Response
    let wide_outcome = wide::evaluate(&wide_builder, &wide_response, gate_threshold, sizes).map_err(|e| {
        failure(10, "invalid-provider-response", format!("Wide evaluation failed: {e:?}"))
    })?;

    let shortlisted = match &wide_outcome.decision {
        WideDecision::LowNeed => {
            // Revalidate policy before emitting LowNeed abstention
            let (refreshed, reval) = config_files.refresh(
                clock,
                &resolved_config,
                &current_receipt,
                PolicyBoundary::CliPublication(PublicationKind::Advisory),
            )?;
            if let Revalidation::Superseded(fields) = reval {
                return Err(failure(
                    3,
                    "superseded",
                    format!("Policy changed during evaluation: {fields:?}"),
                ));
            }
            if let Revalidation::InvalidConfiguration = reval {
                return Err(failure(
                    2,
                    "invalid-configuration",
                    "Configuration became invalid before publication",
                ));
            }
            let _ = refreshed;

            let doc = build_abstain_document(
                "low-need",
                &normalized_context,
                &roster,
                clock.now().as_millis(),
                None,
            );
            return Ok(doc);
        }
        WideDecision::Shortlist(list) => list.clone(),
    };

    let shortlist_ids: Vec<SkillId> = shortlisted.iter().map(|s| s.skill.binding.id.clone()).collect();

    // 12. Stage 2 (Rerank)
    let rerank_builder = rerank::build(
        &roster,
        &shortlist_ids,
        &rendered_context,
        effective.model().as_str(),
    ).map_err(|e| failure(e.kind().exit_code() as u8, e.kind().as_str(), format!("Rerank build failed: {e:?}")))?;

    let rerank_response = execute_provider_rerank(
        clock,
        cx,
        &config_files,
        &resolved_config,
        &mut current_receipt,
        &args.gate,
        &rerank_builder,
        transport,
        &mut metrics,
    ).await?;

    let rerank_outcome = rerank::evaluate(&rerank_builder, &rerank_response).map_err(|e| {
        failure(10, "invalid-provider-response", format!("Rerank evaluation failed: {e:?}"))
    })?;

    // 13. Local Eligibility & Fit Filtering after Rerank
    let shortlisted_advisory: Vec<AdvisorySkill> = shortlisted.iter().map(|s| s.skill).collect();
    let raw_estimates = rerank_outcome.estimates();

    let evaluation = after_rerank(
        &shortlisted_advisory,
        &raw_estimates,
        rerank_outcome.none_probability,
        fits_threshold,
        &excluded_refs,
        loaded_state,
    );

    if let Some(verdict) = evaluation.verdict {
        match verdict {
            Verdict::Abstain(reason) => {
                let doc = build_abstain_document(
                    reason.as_str(),
                    &normalized_context,
                    &roster,
                    clock.now().as_millis(),
                    None,
                );
                return Ok(doc);
            }
            Verdict::Unavailable(reason) => {
                let doc = OutputDocument::failure_with_details(
                    reason.kind(),
                    &format!("Candidate unavailable after rerank: {}", reason.as_str()),
                    "Check candidate eligibility after rerank.",
                    false,
                );
                return Ok(doc);
            }
        }
    }

    // 14. Scoring of surviving eligible candidates
    let scoring_inputs: Vec<ScoringInput> = evaluation
        .eligible
        .iter()
        .map(ScoringInput::from_eligible)
        .collect();

    let weights = Weights::DEFAULT;
    let scored_ranking = rank(&scoring_inputs, weights, top).map_err(|e| {
        failure(10, "invalid-provider-response", format!("Scoring failed: {e:?}"))
    })?;

    // 15. Final Revalidation Before Publication
    // a. Roster dependencies
    let reval_outcome = revalidate_claude(
        &dependencies,
        &args.workspace,
        args.user_config_root.as_deref(),
        visibility,
        &overrides,
        cx,
        clock,
    );
    match reval_outcome {
        Ok(_) => {}
        Err(RevalidationError::Changed) => {
            return Err(failure(5, "roster-changed", "Roster changed during ranking"));
        }
        Err(RevalidationError::Incomplete) => {
            return Err(failure(5, "incomplete-roster", "Roster scope incomplete during revalidation"));
        }
        Err(RevalidationError::Deadline) => {
            return Err(failure(6, "timeout", "Deadline exceeded during revalidation"));
        }
        Err(e) => {
            return Err(failure(5, "unusable-roster", format!("Revalidation failed: {e:?}")));
        }
    }

    // b. Policy receipt revalidation
    let (refreshed, reval) = config_files.refresh(
        clock,
        &resolved_config,
        &current_receipt,
        PolicyBoundary::CliPublication(PublicationKind::Advisory),
    )?;
    if let Revalidation::Superseded(fields) = reval {
        return Err(failure(
            3,
            "superseded",
            format!("Policy superseded before publication: {fields:?}"),
        ));
    }
    if let Revalidation::InvalidConfiguration = reval {
        return Err(failure(
            2,
            "invalid-configuration",
            "Configuration became invalid before publication",
        ));
    }
    let _ = refreshed;

    // Check runtime publication admission (deadline cleanup reserve check)
    admit_publication(clock.deadline(), clock.now(), clock.now()).map_err(|e| {
        failure(6, "timeout", format!("Runtime suppressed late result: {e}"))
    })?;

    // 16. Build Ranked OutputDocument
    let doc = build_ranked_document(
        &evaluation.eligible,
        &scored_ranking,
        &shortlisted,
        &wide_outcome,
        &rerank_outcome,
        &normalized_context,
        &roster,
        &metrics,
        effective.model().as_str(),
        clock.now().as_millis(),
    )?;

    Ok(doc)
}

async fn execute_provider_wide(
    clock: &EntryClock,
    cx: &Cx,
    config_files: &ConfigFiles,
    resolved_config: &ResolvedConfig,
    receipt: &mut PolicyReceipt,
    gate: &EffectGate,
    wide_req: &WideRequest<'_>,
    transport: Option<&dyn JevTransport>,
    metrics: &mut ExecutionMetrics,
) -> Result<Response, PipelineFailure> {
    // Revalidate PolicyReceipt before HTTP attempt
    let (refreshed, reval) = config_files.refresh(
        clock,
        resolved_config,
        receipt,
        PolicyBoundary::ProviderAdmission,
    )?;
    if let Revalidation::Superseded(fields) = reval {
        return Err(failure(
            3,
            "superseded",
            format!("Policy changed before wide send: {fields:?}"),
        ));
    }
    if let Revalidation::InvalidConfiguration = reval {
        return Err(failure(
            2,
            "invalid-configuration",
            "Configuration invalid before wide send",
        ));
    }
    *receipt = refreshed.receipt(gate.policy());

    let consent = receipt.network_consent();
    if !matches!(consent, NetworkConsent::Authorized(_)) {
        return Err(failure(8, "network-denied", "Network transmission is not authorized"));
    }

    let credential = resolved_config.credential();
    if credential.is_none() {
        return Err(failure(4, "authentication", "Missing TYPESAFE_API_KEY"));
    }

    metrics.requests += 1;
    metrics.http_attempts += 1;

    let response = if let Some(t) = transport {
        t.send(wide_req.request(), None, consent, cx, clock)
            .await
            .map_err(map_transport_error)?
    } else {
        let endpoint_config = match resolved_config.effective().endpoint() {
            Some(ep) => EndpointConfig::from_override(ep)
                .map_err(|e| failure(2, "invalid-configuration", format!("Invalid endpoint: {e:?}")))?,
            None => EndpointConfig::production(),
        };
        let client = JevClient::new(endpoint_config)
            .map_err(|e| failure(2, "invalid-configuration", format!("Client init failed: {e}")))?;
        let scoped_cred = OriginScopedCredential::bind(credential.unwrap().clone(), client.origin())
            .map_err(|_| failure(4, "authentication", "Credential origin mismatch"))?;
        client
            .send(wide_req.request(), Some(&scoped_cred), consent, cx, clock)
            .await
            .map_err(map_transport_error)?
    };

    metrics.input_tokens += response.usage.input_tokens;
    metrics.output_tokens += response.usage.output_tokens;

    Ok(response)
}

async fn execute_provider_rerank(
    clock: &EntryClock,
    cx: &Cx,
    config_files: &ConfigFiles,
    resolved_config: &ResolvedConfig,
    receipt: &mut PolicyReceipt,
    gate: &EffectGate,
    rerank_req: &RerankRequest<'_>,
    transport: Option<&dyn JevTransport>,
    metrics: &mut ExecutionMetrics,
) -> Result<Response, PipelineFailure> {
    // Revalidate PolicyReceipt before Rerank HTTP attempt
    let (refreshed, reval) = config_files.refresh(
        clock,
        resolved_config,
        receipt,
        PolicyBoundary::ProviderAdmission,
    )?;
    if let Revalidation::Superseded(fields) = reval {
        return Err(failure(
            3,
            "superseded",
            format!("Policy changed before rerank send: {fields:?}"),
        ));
    }
    if let Revalidation::InvalidConfiguration = reval {
        return Err(failure(
            2,
            "invalid-configuration",
            "Configuration invalid before rerank send",
        ));
    }
    *receipt = refreshed.receipt(gate.policy());

    let consent = receipt.network_consent();
    if !matches!(consent, NetworkConsent::Authorized(_)) {
        return Err(failure(8, "network-denied", "Network transmission is not authorized"));
    }

    let credential = resolved_config.credential();
    if credential.is_none() {
        return Err(failure(4, "authentication", "Missing TYPESAFE_API_KEY"));
    }

    metrics.requests += 1;
    metrics.http_attempts += 1;

    let response = if let Some(t) = transport {
        t.send(rerank_req.request(), None, consent, cx, clock)
            .await
            .map_err(map_transport_error)?
    } else {
        let endpoint_config = match resolved_config.effective().endpoint() {
            Some(ep) => EndpointConfig::from_override(ep)
                .map_err(|e| failure(2, "invalid-configuration", format!("Invalid endpoint: {e:?}")))?,
            None => EndpointConfig::production(),
        };
        let client = JevClient::new(endpoint_config)
            .map_err(|e| failure(2, "invalid-configuration", format!("Client init failed: {e}")))?;
        let scoped_cred = OriginScopedCredential::bind(credential.unwrap().clone(), client.origin())
            .map_err(|_| failure(4, "authentication", "Credential origin mismatch"))?;
        client
            .send(rerank_req.request(), Some(&scoped_cred), consent, cx, clock)
            .await
            .map_err(map_transport_error)?
    };

    metrics.input_tokens += response.usage.input_tokens;
    metrics.output_tokens += response.usage.output_tokens;

    Ok(response)
}

fn map_transport_error(e: TransportError) -> PipelineFailure {
    match e.kind {
        TransportErrorKind::Deadline => (6, "timeout", "Jev request exceeded deadline".into()),
        TransportErrorKind::Admission(_) => (8, "network-denied", "Provider admission denied".into()),
        TransportErrorKind::CredentialOriginMismatch => (4, "authentication", "Credential origin mismatch".into()),
        TransportErrorKind::HttpStatus(401 | 403) => (4, "authentication", "Invalid credentials".into()),
        TransportErrorKind::HttpStatus(429) => (4, "provider-cooldown", "Rate limited by provider".into()),
        TransportErrorKind::HttpStatus(code) => (4, "provider-failure", format!("HTTP error {code}")),
        _ => (4, "network-failure", format!("Transport error: {e}")),
    }
}

fn build_explicit_document(
    skills: &[ResolvedExplicitSkill],
    context: &NormalizedContext,
    roster: &ResolvedRoster,
    elapsed_ms: u64,
) -> OutputDocument {
    let skill_values: Vec<Value> = skills
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let sk = roster.skills().iter().find(|sk| sk.record().id == s.id);
            let (name, path_str, content_hash) = if let Some(sk) = sk {
                let rec = sk.record();
                let p = match &rec.target {
                    LoadTarget::File(p) => p.as_path().display().to_string(),
                    LoadTarget::Harness(h) => h.as_str().to_string(),
                };
                (rec.display_name.as_str(), p, rec.source_content.as_str().to_string())
            } else {
                (s.invocation.as_str(), String::new(), String::new())
            };
            json!({
                "rank": i + 1,
                "skill_id": s.id.as_str(),
                "name": name,
                "invocation_name": s.invocation.as_str(),
                "rank_score": null,
                "rerank_probability": null,
                "wide_probability": null,
                "fits": null,
                "path": path_str,
                "content_hash": content_hash,
            })
        })
        .collect();

    let event_id = context
        .current_request
        .event_id
        .as_ref()
        .map(|e| e.as_str())
        .unwrap_or("event-0");

    let val = json!({
        "schema_version": SCHEMA_VERSION,
        "event_id": event_id,
        "decision": "explicit",
        "reason": "user-required",
        "harness": context.harness.as_str(),
        "context_quality": "complete",
        "quality": {
            "prompt_complete": true,
            "task_anchor_known": true,
            "history_windowed": true,
            "attachments_omitted": context.current_request.attachments_omitted,
            "source_gaps": false,
        },
        "roster": {
            "total": roster.skills().len(),
            "eligible": roster.skills().len(),
            "wide_candidates": 0,
            "shortlist": 0,
            "partial": roster.is_partial(),
            "retrieval": "not-evaluated",
            "provenance": {
                "snapshot_id": "000000000000000000000000000000000000000000000000000000000000000a",
                "policy_version": "ranking-v1",
                "wide_set_id": null,
                "rerank_set_id": null,
            }
        },
        "needs_skill": null,
        "choice_confidence": null,
        "none_probability": null,
        "phase": null,
        "skills": skill_values,
        "omitted_rank_mass": null,
        "cache": {
            "hit": false,
            "wide_hit": false,
            "rerank_hit": false,
            "age_ms": null,
            "stale": false,
        },
        "model": {
            "requested": null,
            "wide_returned": null,
            "rerank_returned": null,
            "immutable_revision": null,
        },
        "usage": {
            "requests": 0,
            "http_attempts": 0,
            "input_tokens": 0,
            "output_tokens": 0,
            "unknown_usage_attempts": 0,
        },
        "persistence": "disabled",
        "warnings": [],
        "warnings_omitted": 0,
        "elapsed_ms": elapsed_ms,
    });

    OutputDocument::from_value(val).expect("valid explicit document")
}

fn build_abstain_document(
    reason: &str,
    context: &NormalizedContext,
    roster: &ResolvedRoster,
    elapsed_ms: u64,
    dry_run: Option<Value>,
) -> OutputDocument {
    let event_id = context
        .current_request
        .event_id
        .as_ref()
        .map(|e| e.as_str())
        .unwrap_or("event-0");

    let mut val = json!({
        "schema_version": SCHEMA_VERSION,
        "event_id": event_id,
        "decision": "abstain",
        "reason": reason,
        "harness": context.harness.as_str(),
        "context_quality": "complete",
        "quality": {
            "prompt_complete": true,
            "task_anchor_known": true,
            "history_windowed": true,
            "attachments_omitted": context.current_request.attachments_omitted,
            "source_gaps": false,
        },
        "roster": {
            "total": roster.skills().len(),
            "eligible": roster.skills().len(),
            "wide_candidates": 0,
            "shortlist": 0,
            "partial": roster.is_partial(),
            "retrieval": "not-evaluated",
            "provenance": {
                "snapshot_id": "000000000000000000000000000000000000000000000000000000000000000a",
                "policy_version": "ranking-v1",
                "wide_set_id": null,
                "rerank_set_id": null,
            }
        },
        "needs_skill": null,
        "choice_confidence": null,
        "none_probability": null,
        "phase": null,
        "skills": [],
        "omitted_rank_mass": null,
        "cache": {
            "hit": false,
            "wide_hit": false,
            "rerank_hit": false,
            "age_ms": null,
            "stale": false,
        },
        "model": {
            "requested": null,
            "wide_returned": null,
            "rerank_returned": null,
            "immutable_revision": null,
        },
        "usage": {
            "requests": 0,
            "http_attempts": 0,
            "input_tokens": 0,
            "output_tokens": 0,
            "unknown_usage_attempts": 0,
        },
        "persistence": "disabled",
        "warnings": [],
        "warnings_omitted": 0,
        "elapsed_ms": elapsed_ms,
    });

    if let Some(dr) = dry_run {
        val["dry_run"] = dr;
    }

    OutputDocument::from_value(val).expect("valid abstain document")
}

fn build_ranked_document(
    eligible: &[Eligible<'_>],
    ranking: &Ranking,
    shortlisted: &[wide::Shortlisted<'_>],
    wide_outcome: &WideOutcome<'_>,
    rerank_outcome: &RerankOutcome<'_>,
    context: &NormalizedContext,
    roster: &ResolvedRoster,
    metrics: &ExecutionMetrics,
    model: &str,
    elapsed_ms: u64,
) -> Result<OutputDocument, PipelineFailure> {
    let mut skill_values = Vec::new();
    for (i, scored) in ranking.returned.iter().enumerate() {
        let el = &eligible[scored.index];
        let wide_prob = shortlisted
            .iter()
            .find(|s| s.skill.binding.id == el.skill.binding.id)
            .map(|s| s.wide_probability)
            .unwrap_or(0.0);

        let rec = el.skill.record;
        let path_str = match &rec.target {
            LoadTarget::File(p) => p.as_path().display().to_string(),
            LoadTarget::Harness(h) => h.as_str().to_string(),
        };

        skill_values.push(json!({
            "rank": i + 1,
            "skill_id": el.skill.binding.id.as_str(),
            "name": rec.display_name.as_str(),
            "invocation_name": el.skill.binding.invocation.as_str(),
            "rank_score": scored.rank_score,
            "rerank_probability": el.rerank,
            "wide_probability": wide_prob,
            "fits": el.fit,
            "path": path_str,
            "content_hash": rec.source_content.as_str(),
        }));
    }

    let event_id = context
        .current_request
        .event_id
        .as_ref()
        .map(|e| e.as_str())
        .unwrap_or("event-0");

    let phase_str = wide_outcome
        .phase
        .iter()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map(|(p, _)| p.as_str())
        .unwrap_or("other");

    let val = json!({
        "schema_version": SCHEMA_VERSION,
        "event_id": event_id,
        "decision": "ranked",
        "reason": "eligible-candidates",
        "harness": context.harness.as_str(),
        "context_quality": "complete",
        "quality": {
            "prompt_complete": true,
            "task_anchor_known": true,
            "history_windowed": true,
            "attachments_omitted": context.current_request.attachments_omitted,
            "source_gaps": false,
        },
        "roster": {
            "total": roster.skills().len(),
            "eligible": eligible.len(),
            "wide_candidates": shortlisted.len(),
            "shortlist": shortlisted.len(),
            "partial": roster.is_partial(),
            "retrieval": "full",
            "provenance": {
                "snapshot_id": "000000000000000000000000000000000000000000000000000000000000000a",
                "policy_version": "ranking-v1",
                "wide_set_id": "000000000000000000000000000000000000000000000000000000000000000b",
                "rerank_set_id": "000000000000000000000000000000000000000000000000000000000000000c",
            }
        },
        "needs_skill": wide_outcome.needs_skill,
        "choice_confidence": rerank_outcome.choice_confidence,
        "none_probability": rerank_outcome.none_probability,
        "phase": phase_str,
        "skills": skill_values,
        "omitted_rank_mass": ranking.omitted_mass,
        "cache": {
            "hit": metrics.cache_hit,
            "wide_hit": metrics.wide_hit,
            "rerank_hit": metrics.rerank_hit,
            "age_ms": metrics.cache_age_ms,
            "stale": false,
        },
        "model": {
            "requested": model,
            "wide_returned": model,
            "rerank_returned": model,
            "immutable_revision": null,
        },
        "usage": {
            "requests": metrics.requests,
            "http_attempts": metrics.http_attempts,
            "input_tokens": metrics.input_tokens,
            "output_tokens": metrics.output_tokens,
            "unknown_usage_attempts": metrics.unknown_usage_attempts,
        },
        "persistence": "disabled",
        "warnings": [],
        "warnings_omitted": 0,
        "elapsed_ms": elapsed_ms,
    });

    OutputDocument::from_value(val).map_err(|e| {
        failure(10, "invalid-provider-response", format!("Output contract validation failed: {e:?}"))
    })
}
