#![cfg(target_os = "linux")]
//! P2 acceptance gate: every eligible output of discovery, resolution and
//! retrieval maps to a currently authorized local record. Each admitted option
//! names a file inside a documented root whose present bytes hash to the
//! recorded content. Manual-only records never become options, a malformed
//! or escaping record leaves no eligible output at all, and a changed file is
//! detected rather than reused.
use asupersync::Cx;
use skillranker::authorized_read::{AuthorizedRoot, AuthorizedRoots};
use skillranker::identity::SkillId;
use skillranker::limits::{DurationMillis, SKILL_FILE_BYTES};
use skillranker::roster::discovery::claude_code_plan;
use skillranker::roster::resolution::{
    AdvisorySkill, OptionMap, ResolvedOption, ResolvedRoster, resolve_claude_plan,
};
use skillranker::roster::retrieval::{QueryInput, RetrievalBudget, RetrievalError, retrieve};
use skillranker::roster::revalidation::{RevalidationError, capture, revalidate_claude};
use skillranker::roster::{LoadTarget, Visibility};
use skillranker::runtime::{EntryClock, ProcessInvocation};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn visibility() -> Visibility {
    Visibility::Verified {
        contract_version: "test-adapter-v1".into(),
    }
}

// Intentionally retained: repository policy forbids automatic tree deletion.
fn tree() -> PathBuf {
    // Trees are retained, so a reused PID must never reuse an old tree.
    let path = std::env::temp_dir().join(format!(
        "sr-p2-gate-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&path).unwrap();
    path
}

fn write(root: &Path, name: &str, frontmatter: &str) -> PathBuf {
    let dir = root.join(name);
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("SKILL.md");
    fs::write(
        &file,
        format!("---\nname: {name}\ndescription: gateword helps with {name}.\n{frontmatter}---\nBody of {name}.\n"),
    )
    .unwrap();
    file
}

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn workspace(&self) -> PathBuf {
        self.root.join("workspace")
    }
    fn home(&self) -> PathBuf {
        self.root.join("home")
    }
    fn project(&self) -> PathBuf {
        self.workspace().join(".claude/skills")
    }
    fn personal(&self) -> PathBuf {
        self.home().join(".claude/skills")
    }
    /// Advisory candidates plus every kind of record that must not be one.
    fn new(ordinary: usize) -> Self {
        let f = Self { root: tree() };
        fs::create_dir_all(f.project()).unwrap();
        fs::create_dir_all(f.personal()).unwrap();
        for n in 0..ordinary {
            write(&f.project(), &format!("skill-{n:04}"), "");
        }
        write(&f.personal(), "personal-only", "");
        write(&f.project(), "manual", "disable-model-invocation: true\n");
        f
    }
    /// Unclosed frontmatter: an excluded record, never an empty skill.
    fn add_malformed(&self) {
        let broken = self.project().join("broken");
        fs::create_dir_all(&broken).unwrap();
        fs::write(
            broken.join("SKILL.md"),
            "---\nname: broken\ndescription: gateword\n",
        )
        .unwrap();
    }
    /// A skill file that is a symlink leaving the authorized roots.
    /// Named `aa-escape` so it sorts before other records, verifying
    /// that read errors do not abort inspection of subsequent files.
    fn add_escape(&self) -> PathBuf {
        let outside = self.root.join("outside");
        let target = write(&outside, "escape", "");
        let escape = self.project().join("aa-escape");
        fs::create_dir_all(&escape).unwrap();
        std::os::unix::fs::symlink(&target, escape.join("SKILL.md")).unwrap();
        target
    }
    /// A malformed skill file sharing an invocation name with an existing project skill.
    fn add_same_name_malformed(&self, name: &str) {
        let dir = self.personal().join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: gateword malformed\n"),
        )
        .unwrap();
    }
    /// An unsupported layout whose invocation name is unknowable.
    fn add_unsupported_layout(&self) {
        let dir = self.project().join("nested").join("deep");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            "---\nname: nested\ndescription: gateword\n---\nbody\n",
        )
        .unwrap();
    }
    fn resolve(&self, clock: &EntryClock, cx: &Cx) -> ResolvedRoster {
        let plan = claude_code_plan(&self.workspace(), Some(&self.home()), visibility()).unwrap();
        resolve_claude_plan(&plan, &BTreeMap::new(), cx, clock).unwrap()
    }
    fn roots(&self) -> AuthorizedRoots {
        AuthorizedRoots::new(vec![
            AuthorizedRoot::open_absolute(&self.project()).unwrap(),
            AuthorizedRoot::open_absolute(&self.personal()).unwrap(),
        ])
    }
}

fn runtime() -> (EntryClock, ProcessInvocation, Cx) {
    let clock = EntryClock::capture_with(
        DurationMillis::new("test-total", 30_000, 60_000).unwrap(),
        DurationMillis::new("test-cleanup", 100, 60_000).unwrap(),
    )
    .unwrap();
    let invocation = ProcessInvocation::from_clock(clock).unwrap();
    let cx = invocation.request_cx().unwrap();
    (clock, invocation, cx)
}

/// The option's file is inside an authorized root and its present bytes are
/// exactly the recorded content.
fn assert_currently_authorized(roots: &AuthorizedRoots, skill: &AdvisorySkill<'_>) {
    let LoadTarget::File(path) = &skill.record.target else {
        panic!(
            "{}: a Claude skill loads from a file",
            skill.record.id.as_str()
        );
    };
    let read = roots
        .read_absolute(path.as_path(), SKILL_FILE_BYTES)
        .unwrap_or_else(|error| panic!("{}: {error:?}", skill.record.id.as_str()));
    assert_eq!(read.content_hash(), &skill.record.source_content);
    assert!(skill.record.restrictions.agent_invocable);
}

fn names(options: &OptionMap<'_>) -> BTreeSet<String> {
    options
        .entries()
        .values()
        .map(|s| s.record.invocation_name.as_str().to_owned())
        .collect()
}

#[test]
fn eligible_output_maps_to_currently_authorized_local_records() {
    // 3 fits the full-roster path; 300 forces Quill admission of at most 254.
    for ordinary in [3, 300] {
        let f = Fixture::new(ordinary);
        let (clock, invocation, cx) = runtime();
        let roster = f.resolve(&clock, &cx);
        let roots = f.roots();
        let selection = invocation
            .runtime()
            .block_on(retrieve(
                &roster,
                &BTreeSet::new(),
                QueryInput {
                    latest_request: "gateword",
                    active_task: "",
                    recent_errors: "",
                },
                RetrievalBudget::default(),
                &cx,
                &clock,
            ))
            .unwrap();
        let admitted: Vec<SkillId> = selection
            .candidates
            .iter()
            .map(|c| c.record.id.clone())
            .collect();
        assert_eq!(
            admitted.len(),
            (ordinary + 1).min(254),
            "ordinary={ordinary}"
        );
        let options = OptionMap::new(&roster, &admitted).unwrap();
        assert_eq!(options.entries().len(), admitted.len());
        for skill in options.entries().values() {
            assert_currently_authorized(&roots, skill);
        }
        let offered = names(&options);
        assert!(!offered.contains("manual"), "manual-only became an option");
        if ordinary < 254 {
            assert!(offered.contains("personal-only"));
        }
        // Option IDs resolve only through the local map; the sentinel is none.
        assert!(matches!(
            options.resolve("__none__"),
            Ok(ResolvedOption::None)
        ));
        assert!(options.resolve("o999").is_err());
        // Excluded records cannot be forced in as options either.
        let manual = roster
            .skills()
            .iter()
            .find(|s| s.record().invocation_name.as_str() == "manual")
            .map(|s| s.record().id.clone())
            .expect("manual-only skill is resolved, not dropped");
        assert!(OptionMap::new(&roster, &[manual]).is_err());

        // A changed option is caught before it could be published as current.
        let captured = capture(&roster, &admitted, &clock).unwrap();
        let first = options.entries().values().next().unwrap();
        let LoadTarget::File(path) = &first.record.target else {
            unreachable!()
        };
        fs::write(
            path.as_path(),
            "---\ndescription: gateword changed.\n---\nNew body.\n",
        )
        .unwrap();
        let fresh = roots
            .read_absolute(path.as_path(), SKILL_FILE_BYTES)
            .unwrap();
        assert_ne!(fresh.content_hash(), &first.record.source_content);
        assert_eq!(
            revalidate_claude(
                &captured,
                &f.workspace(),
                Some(&f.home()),
                visibility(),
                &BTreeMap::new(),
                &cx,
                &clock,
            ),
            Err(RevalidationError::Changed)
        );
        assert!(invocation.shutdown());
    }
}

fn retrieve_all(
    roster: &ResolvedRoster,
    invocation: &ProcessInvocation,
    cx: &Cx,
    clock: &EntryClock,
) -> Result<Vec<SkillId>, RetrievalError> {
    invocation
        .runtime()
        .block_on(retrieve(
            roster,
            &BTreeSet::new(),
            QueryInput {
                latest_request: "gateword",
                active_task: "",
                recent_errors: "",
            },
            RetrievalBudget::default(),
            cx,
            clock,
        ))
        .map(|selection| {
            selection
                .candidates
                .iter()
                .map(|c| c.record.id.clone())
                .collect()
        })
        .map_err(|failure| failure.kind)
}

/// One malformed or escaping file among valid ones leaves the other names
/// advisory and inspectable, while a same-name malformed winner still withholds
/// that name. An unknowable layout withholds authority globally.
#[test]
fn unreadable_or_malformed_records_leave_no_eligible_output() {
    for hostile in ["malformed", "escape"] {
        let f = Fixture::new(3);
        let (clock, invocation, cx) = runtime();
        let before = f.resolve(&clock, &cx);
        assert_eq!(
            retrieve_all(&before, &invocation, &cx, &clock)
                .unwrap()
                .len(),
            4
        );
        let outside = match hostile {
            "malformed" => {
                f.add_malformed();
                None
            }
            _ => Some(f.add_escape()),
        };
        let roster = f.resolve(&clock, &cx);
        assert!(roster.is_partial(), "{hostile}");
        assert!(!roster.diagnostics().is_empty(), "{hostile}");
        // One malformed or escaping file leaves valid other names advisory and inspectable
        let admitted = retrieve_all(&roster, &invocation, &cx, &clock).unwrap();
        assert_eq!(admitted.len(), 4, "{hostile}");
        let options = OptionMap::new(&roster, &admitted).unwrap();
        let offered = names(&options);
        for excluded in ["broken", "aa-escape", "manual"] {
            assert!(
                !offered.contains(excluded),
                "{hostile}: {excluded} became an option"
            );
        }
        if let Some(outside) = outside {
            // The escaped file's bytes never enter any record.
            let escaped =
                skillranker::identity::ContentHash::from_bytes(&fs::read(outside).unwrap());
            assert!(
                roster
                    .skills()
                    .iter()
                    .all(|s| s.record().source_content != escaped)
            );
        }
        assert!(invocation.shutdown());
    }

    // A same-name malformed winner in personal withholds authority for that specific name
    {
        let f = Fixture::new(3);
        let (clock, invocation, cx) = runtime();
        f.add_same_name_malformed("skill-0000");
        let roster = f.resolve(&clock, &cx);
        assert!(roster.is_partial());
        assert!(!roster.diagnostics().is_empty());
        let admitted = retrieve_all(&roster, &invocation, &cx, &clock).unwrap();
        // skill-0000 is withheld; skill-0001, skill-0002, and personal-only remain
        assert_eq!(admitted.len(), 3);
        let options = OptionMap::new(&roster, &admitted).unwrap();
        let offered = names(&options);
        assert!(!offered.contains("skill-0000"));
        assert_eq!(
            roster.exact_name("skill-0000"),
            skillranker::roster::resolution::ExactResolution::Unverified
        );
        let skill_0000 = roster
            .skills()
            .iter()
            .find(|s| s.record().invocation_name.as_str() == "skill-0000")
            .map(|s| s.record().id.clone())
            .unwrap();
        assert!(OptionMap::new(&roster, &[skill_0000]).is_err());
        assert!(invocation.shutdown());
    }

    // An unsupported layout whose name is unknowable withholds authority globally
    {
        let f = Fixture::new(3);
        let (clock, invocation, cx) = runtime();
        f.add_unsupported_layout();
        let roster = f.resolve(&clock, &cx);
        assert!(roster.is_partial());
        assert_eq!(
            retrieve_all(&roster, &invocation, &cx, &clock).unwrap_err(),
            RetrievalError::NoEligibleCandidates
        );
        for skill in roster.skills() {
            assert!(
                OptionMap::new(&roster, std::slice::from_ref(&skill.record().id)).is_err(),
                "nested: {} became an option",
                skill.record().invocation_name.as_str()
            );
        }
        assert!(invocation.shutdown());
    }
}
