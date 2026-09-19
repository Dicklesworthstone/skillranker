//! Reload the same selected inventory at capture and publication boundaries.
use super::{PROVISIONAL_CLAUDE_CONTRACT, PipelineFailure, failure};
use crate::identity::SkillId;
use crate::roster::Visibility;
use crate::roster::discovery::claude_code_plan;
use crate::roster::import::{ImportError, import_authorized, read_roster_file};
use crate::roster::resolution::{ResolutionError, ResolvedRoster, resolve_claude_plan};
use crate::roster::revalidation::{Dependencies, RevalidationError, capture, revalidate};
use crate::runtime::EntryClock;
use asupersync::Cx;
use std::collections::BTreeMap;
use std::path::Path;

pub(super) struct Source<'a> {
    pub workspace: &'a Path,
    pub home: Option<&'a Path>,
    pub manifest: Option<&'a Path>,
}

impl Source<'_> {
    pub(super) fn load(
        &self,
        cx: &Cx,
        clock: &EntryClock,
    ) -> Result<ResolvedRoster, PipelineFailure> {
        clock
            .admit_new_work()
            .map_err(|_| validation_error(RevalidationError::Deadline))?;
        let plan = claude_code_plan(
            self.workspace,
            self.home,
            Visibility::Verified {
                contract_version: PROVISIONAL_CLAUDE_CONTRACT.into(),
            },
        )
        .map_err(|_| failure(5, "unusable-roster", "Failed to create roster source plan"))?;
        let overrides = BTreeMap::new();
        match self.manifest {
            Some(path) => {
                let path = if path.is_absolute() {
                    path.to_owned()
                } else {
                    self.workspace.join(path)
                };
                let bytes = read_roster_file(&path).map_err(import_error)?;
                import_authorized(&bytes, &plan, &overrides, cx, clock).map_err(import_error)
            }
            None => resolve_claude_plan(&plan, &overrides, cx, clock).map_err(|error| {
                if error == ResolutionError::Deadline {
                    validation_error(RevalidationError::Deadline)
                } else {
                    failure(
                        5,
                        "unusable-roster",
                        format!("Failed to resolve roster: {error:?}"),
                    )
                }
            }),
        }
    }

    pub(super) fn validate(
        &self,
        captured: &Dependencies,
        cx: &Cx,
        clock: &EntryClock,
    ) -> Result<(), PipelineFailure> {
        // Re-open both the manifest and adapter roots; old open descriptors
        // cannot establish that a replacement still has the same authority.
        let fresh = self.load(cx, clock)?;
        revalidate(captured, &fresh, clock).map_err(validation_error)?;
        Ok(())
    }
}

pub(super) fn capture_dependencies<'a>(
    roster: &ResolvedRoster,
    content: impl IntoIterator<Item = &'a SkillId>,
    clock: &EntryClock,
) -> Result<Dependencies, PipelineFailure> {
    capture(roster, content, clock).map_err(validation_error)
}

fn import_error(error: ImportError) -> PipelineFailure {
    if matches!(
        error,
        ImportError::Deadline | ImportError::Resolution(ResolutionError::Deadline)
    ) {
        validation_error(RevalidationError::Deadline)
    } else {
        failure(
            5,
            "unusable-roster",
            format!("Failed to import roster: {error}"),
        )
    }
}

fn validation_error(error: RevalidationError) -> PipelineFailure {
    match error {
        RevalidationError::Changed => {
            failure(5, "roster-changed", "Roster changed before publication")
        }
        RevalidationError::Incomplete => failure(
            5,
            "incomplete-roster",
            "Roster scope incomplete during revalidation",
        ),
        RevalidationError::Deadline => {
            failure(6, "timeout", "Deadline exceeded during roster validation")
        }
        other => failure(
            5,
            "unusable-roster",
            format!("Roster validation failed: {other:?}"),
        ),
    }
}
