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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::DurationMillis;
    use crate::roster::resolution::ExactResolution;
    use crate::runtime::ProcessInvocation;
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        root: PathBuf,
        clock: EntryClock,
        invocation: ProcessInvocation,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "sr-publication-roster-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed),
            ));
            fs::create_dir_all(&root).unwrap();
            let clock = EntryClock::capture_with(
                DurationMillis::new("test", 30_000, 30_000).unwrap(),
                DurationMillis::new("cleanup", 200, 30_000).unwrap(),
            )
            .unwrap();
            let invocation = ProcessInvocation::from_clock(clock).unwrap();
            let fixture = Self {
                root,
                clock,
                invocation,
            };
            fixture.skill(
                "alpha",
                "disable-model-invocation: true\n",
                "Original body.",
            );
            fixture.skill("beta", "", "Unrelated body.");
            fixture
        }

        fn source(&self, manifest: bool) -> Source<'_> {
            Source {
                workspace: &self.root,
                home: None,
                manifest: manifest.then_some(Path::new("roster.json")),
            }
        }

        fn skill(&self, name: &str, flags: &str, body: &str) {
            let dir = self.root.join(".claude/skills").join(name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join("SKILL.md"),
                format!(
                    "---\nname: {name}\ndescription: Help review Rust code\n{flags}---\n{body}\n"
                ),
            )
            .unwrap();
        }

        fn manifest(&self, names: &[&str]) {
            let records: Vec<_> = names
                .iter()
                .map(|name| {
                    json!({
                        "source": "claude_code.project", "path": format!("{name}/SKILL.md")
                    })
                })
                .collect();
            fs::write(
                self.root.join("roster.json"),
                serde_json::to_vec(&json!({
                    "schema": "sr.roster.v1", "harness": "claude_code",
                    "mode": "authorized_files", "skills": records
                }))
                .unwrap(),
            )
            .unwrap();
        }

        fn capture(&self, manifest: bool) -> Dependencies {
            let cx = self.invocation.request_cx().unwrap();
            let roster = self.source(manifest).load(&cx, &self.clock).unwrap();
            let id = match roster.exact_name("alpha") {
                ExactResolution::Resolved { id, .. } => id,
                other => panic!("alpha must resolve, including manual-only: {other:?}"),
            };
            capture_dependencies(&roster, [id], &self.clock).unwrap()
        }

        fn validate(&self, manifest: bool, captured: &Dependencies) -> Result<(), PipelineFailure> {
            self.source(manifest).validate(
                captured,
                &self.invocation.request_cx().unwrap(),
                &self.clock,
            )
        }
    }

    #[test]
    fn explicit_target_content_and_restrictions_are_revalidated() {
        for manifest in [false, true] {
            for change_restriction in [false, true] {
                let f = Fixture::new();
                f.manifest(&["alpha"]);
                let captured = f.capture(manifest);
                assert_eq!(f.validate(manifest, &captured), Ok(()));
                if change_restriction {
                    f.skill(
                        "alpha",
                        "disable-model-invocation: true\nuser-invocable: false\n",
                        "Original body.",
                    );
                } else {
                    f.skill(
                        "alpha",
                        "disable-model-invocation: true\n",
                        "Modified body.",
                    );
                }
                assert_eq!(
                    f.validate(manifest, &captured).unwrap_err().1,
                    "roster-changed"
                );
            }
        }
    }

    #[test]
    fn imported_subset_is_not_replaced_by_filesystem_discovery() {
        let f = Fixture::new();
        f.manifest(&["alpha"]);
        let captured = f.capture(true);
        assert_eq!(f.validate(true, &captured), Ok(()));
        f.skill("beta", "user-invocable: false\n", "Changed unlisted skill.");
        f.skill("gamma", "", "New unlisted skill.");
        assert_eq!(f.validate(true, &captured), Ok(()));
        f.manifest(&["alpha", "gamma"]);
        assert_eq!(f.validate(true, &captured).unwrap_err().1, "roster-changed");
    }

    #[test]
    fn discovered_membership_is_revalidated() {
        let f = Fixture::new();
        let captured = f.capture(false);
        assert_eq!(f.validate(false, &captured), Ok(()));
        f.skill("gamma", "", "New discovered skill.");
        assert_eq!(
            f.validate(false, &captured).unwrap_err().1,
            "roster-changed"
        );
    }

    #[test]
    fn replaced_manifest_cannot_fall_back_to_discovery() {
        let f = Fixture::new();
        f.manifest(&["alpha"]);
        let captured = f.capture(true);
        fs::write(f.root.join("roster.json"), b"not json").unwrap();
        assert_eq!(
            f.validate(true, &captured).unwrap_err().1,
            "unusable-roster"
        );
    }

    #[test]
    fn publication_validation_cannot_extend_the_deadline() {
        let f = Fixture::new();
        let captured = f.capture(false);
        let expired = EntryClock::capture_with(
            DurationMillis::new("test", 2, 30_000).unwrap(),
            DurationMillis::new("cleanup", 1, 30_000).unwrap(),
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let failure = f
            .source(false)
            .validate(&captured, &f.invocation.request_cx().unwrap(), &expired)
            .unwrap_err();
        assert_eq!((failure.0, failure.1), (6, "timeout"));
    }
}
