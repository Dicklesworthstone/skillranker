//! `sr rank --session PATH`: read one exact cass session of this workspace.
//!
//! cass is located and qualified without setup (see
//! [`crate::context::cass::default_config`]). The selected path must be a
//! session cass records for this exact workspace; its listing entry also
//! names the agent that produced it, which becomes the session's harness. The
//! export is projected into normalized events. No durable session identity is
//! claimed, so the cache and lease namespace stays private to this run.
use super::{PipelineFailure, failure};
use crate::context::cass::{CassAdapter, CassError, MAX_LISTING, default_config};
use crate::context::source::SourcePolicy;
use crate::context::{CurrentRequest, EventKind, NormalizedContext, PrivateText, Role};
use crate::identity::HarnessId;
use crate::runtime::EntryClock;
use crate::subprocess::SubprocessError;
use asupersync::Cx;
use std::path::Path;

pub(super) async fn read(
    workspace: &Path,
    home: Option<&Path>,
    selected: &Path,
    include_tools: bool,
    policy: SourcePolicy,
    cx: &Cx,
    clock: &EntryClock,
) -> Result<NormalizedContext, PipelineFailure> {
    let data_home = std::env::var_os("XDG_DATA_HOME").map(std::path::PathBuf::from);
    let database = std::env::var_os("CASS_DB_PATH").map(std::path::PathBuf::from);
    let config = default_config(home, data_home.as_deref(), database.as_deref(), workspace)
        .map_err(cass_failure)?;
    let adapter = CassAdapter::probe(config, policy, cx, clock)
        .await
        .map_err(cass_failure)?;
    let selected = if selected.is_absolute() {
        selected.to_path_buf()
    } else {
        workspace.join(selected)
    };
    let listing = adapter
        .sessions(workspace, MAX_LISTING, policy, cx, clock)
        .await
        .map_err(cass_failure)?;
    let Some(entry) = listing
        .sessions
        .into_iter()
        .find(|session| session.selection.path() == selected.as_path())
    else {
        return Err(failure(
            3,
            "missing-session",
            "The selected cass session is not recorded for this workspace",
        ));
    };
    // cass names Claude Code's agent `claude`; other agents keep their name.
    let harness = match entry.harness.as_str() {
        "claude" | "claude_code" | "claude-code" => crate::adapter::CLAUDE_CODE_ID,
        other => other,
    };
    let harness = HarnessId::new(harness).map_err(|_| {
        failure(
            7,
            "unsupported-input",
            "The cass session names an invalid agent",
        )
    })?;
    let export = adapter
        .export(&entry.selection, include_tools, policy, cx, clock)
        .await
        .map_err(cass_failure)?;
    let archive = export.normalized_events().map_err(cass_failure)?;
    let current = archive
        .events
        .iter()
        .rev()
        .find(|event| event.role == Role::User && event.kind == EventKind::Message);
    let current_request = CurrentRequest {
        event_id: current.and_then(|event| event.event_id.clone()),
        text: current.map_or_else(|| PrivateText::new(""), |event| event.text.clone()),
        attachments_omitted: false,
        essential_attachment_missing: false,
    };
    Ok(NormalizedContext {
        schema_version: 1,
        harness,
        producer_id: None,
        workspace_root: PrivateText::new(workspace.to_string_lossy()),
        session_id: None,
        agent_id: None,
        branch_id: None,
        context_epoch: None,
        current_request,
        events: archive.events,
        explicit_skill_references: Vec::new(),
        supplied_loads: Vec::new(),
    })
}

fn cass_failure(error: CassError) -> PipelineFailure {
    let message = error.to_string();
    match error {
        CassError::UnsupportedSourceMode | CassError::NotInstalled => {
            failure(7, "unsupported-source-mode", message)
        }
        CassError::InvalidResponse => failure(7, "malformed-input", message),
        CassError::LimitExceeded => failure(7, "oversized-input", message),
        CassError::Process(SubprocessError::DeadlineExceeded) => failure(6, "timeout", message),
        _ => failure(7, "unsupported-input", message),
    }
}
