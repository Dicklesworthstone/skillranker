#![cfg(any(target_os = "linux", target_os = "macos"))]
//! The central effect gate against real boundaries, for every flag combination:
//! cache files on disk, a cass child process, provider sockets, configuration
//! reads and explicit resolution. Each prohibited effect has an observed
//! positive twin.

use asupersync::Cx;
use serde_json::json;
use skillranker::cli::ConfigFiles;
use skillranker::config::{ConfigSources, RawValue, ResolvedConfig};
use skillranker::context::cass::{CassAdapter, CassConfig, CassError};
use skillranker::context::source::{SelectionOutcome, SourceError, SourceOptions, SourceTarget};
use skillranker::effects::{EffectGate, History, NetworkEffect, Scope, StoreEffect};
use skillranker::identity::{ContentHash, WorkspaceId};
use skillranker::jev::client::{JevClient, TransportErrorKind};
use skillranker::jev::codec::{Question, Request};
use skillranker::jev::{EndpointConfig, OriginScopedCredential};
use skillranker::limits::DurationMillis;
use skillranker::output::{CliExit, ErrorKind};
use skillranker::privacy::{
    ConsentSource, EffectFlags, NetworkBlock, NetworkConsent, ProviderAdmissionRefusal, StoreAccess,
};
use skillranker::roster::discovery::claude_code_plan;
use skillranker::roster::explicit::{
    ExplicitResolutionRequest, ExplicitResolutionResult, resolve_explicit_requirements,
};
use skillranker::roster::resolution::resolve_claude_plan;
use skillranker::roster::{LocalPath, Visibility};
use skillranker::runtime::{EntryClock, ProcessInvocation};
use skillranker::storage::{CACHE_FILE, CacheAccess, CacheLocation, CacheOpen};
use skillranker::subprocess::TrustedExecutable;
use std::ffi::OsString;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::Write;
use std::net::TcpListener;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
const CANARY: &str = "synthetic-effect-gate-canary";

/// All 2^7 flag combinations, including conflicting ones.
fn all_flags() -> Vec<EffectFlags> {
    (0u8..128)
        .map(|bits| EffectFlags {
            offline: bits & 1 != 0,
            allow_network: bits & 2 != 0,
            dry_run: bits & 4 != 0,
            no_cache: bits & 8 != 0,
            no_ledger: bits & 16 != 0,
            no_persist: bits & 32 != 0,
            save_case: bits & 64 != 0,
        })
        .collect()
}

fn conflicting(f: EffectFlags) -> bool {
    (f.offline && f.allow_network)
        || (f.dry_run && f.allow_network)
        || (f.save_case && f.dry_run)
        || (f.save_case && f.no_persist)
}

/// Every valid (flags, scope) gate.
fn gates() -> Vec<(EffectFlags, EffectGate)> {
    let mut gates = Vec::new();
    for flags in all_flags() {
        for scope in [Scope::Rank, Scope::LocalInspection] {
            if let Ok(gate) = EffectGate::new(flags, scope) {
                gates.push((flags, gate));
            }
        }
    }
    gates
}

// Intentionally retained: repository policy forbids automatic tree deletion.
// Root-owned sticky /tmp satisfies the cache's private-ancestor policy even
// where a worker's TMPDIR has group-writable ancestors.
fn private_tree(case: &str) -> PathBuf {
    let path = Path::new("/tmp").join(format!(
        "sr-effects-{case}-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    DirBuilder::new().mode(0o700).create(&path).unwrap();
    path
}

fn entries(path: &Path) -> usize {
    fs::read_dir(path).unwrap().count()
}

struct Run {
    clock: EntryClock,
    invocation: ProcessInvocation,
    cx: Cx,
}

impl Run {
    fn new() -> Self {
        let clock = EntryClock::capture_with(
            DurationMillis::new("test-total", 20_000, 30_000).unwrap(),
            DurationMillis::new("test-cleanup", 100, 30_000).unwrap(),
        )
        .unwrap();
        let invocation = ProcessInvocation::from_clock(clock).unwrap();
        let cx = invocation.request_cx().unwrap();
        Self {
            clock,
            invocation,
            cx,
        }
    }
    fn finish(self) {
        assert!(self.invocation.shutdown(), "owned runtime must shut down");
    }
}

#[test]
fn every_combination_derives_one_restrictive_policy() {
    let mut valid = 0;
    for flags in all_flags() {
        for scope in [Scope::Rank, Scope::LocalInspection] {
            let gate = match EffectGate::new(flags, scope) {
                Ok(gate) => gate,
                Err(conflicts) => {
                    assert!(conflicting(flags), "{flags:?} rejected: {conflicts:?}");
                    assert!(
                        conflicts
                            .iter()
                            .all(|c| c.kind() == ErrorKind::InvalidUsage)
                    );
                    continue;
                }
            };
            assert!(!conflicting(flags), "{flags:?} must be rejected");
            valid += 1;
            let stateless = flags.dry_run || flags.no_persist;
            let first = |own: bool, name: &'static str| {
                if flags.dry_run {
                    StoreEffect::Disabled { by: "dry-run" }
                } else if flags.no_persist {
                    StoreEffect::Disabled { by: "no-persist" }
                } else if own {
                    StoreEffect::Disabled { by: name }
                } else {
                    StoreEffect::Enabled
                }
            };
            let receipt = gate.receipt();
            assert_eq!(receipt.response_cache, first(flags.no_cache, "no-cache"));
            assert_eq!(receipt.ledger, first(flags.no_ledger, "no-ledger"));
            assert_eq!(receipt.runtime_state, first(false, ""));
            assert_eq!(receipt.stateless, stateless);
            assert_eq!(gate.stateless(), stateless);
            assert_eq!(gate.cross_process_coordination(), !stateless);
            assert_eq!(
                gate.history() == History::Available,
                gate.ledger() == StoreAccess::Enabled
            );
            // Offline alone controls the network, never persistence.
            if flags.offline && !stateless && !flags.no_cache && !flags.no_ledger {
                assert_eq!(gate.response_cache(), StoreAccess::Enabled);
                assert_eq!(gate.ledger(), StoreAccess::Enabled);
                assert_eq!(gate.runtime_state(), StoreAccess::Enabled);
            }
            let source = gate.source_policy();
            assert_eq!(
                (source.offline, source.dry_run, source.allow_network),
                (flags.offline, flags.dry_run, flags.allow_network)
            );
            assert_eq!(source.local_only, scope == Scope::LocalInspection);
            let children = !(flags.offline || flags.dry_run || scope == Scope::LocalInspection);
            assert_eq!(gate.unverified_children(), children);
            assert_eq!(receipt.unverified_children, children);
            let network = match (scope, flags.offline, flags.dry_run) {
                (Scope::LocalInspection, _, _) => NetworkEffect::Blocked {
                    by: "local-inspection",
                },
                (Scope::Rank, true, _) => NetworkEffect::Blocked { by: "offline" },
                (Scope::Rank, false, true) => NetworkEffect::Blocked { by: "dry-run" },
                (Scope::Rank, false, false) => NetworkEffect::ConsentRequired,
            };
            assert_eq!(receipt.network, network);
        }
    }
    // 128 combinations: 76 conflicting and 52 valid, in each of two scopes.
    assert_eq!(valid, 2 * 52);
    assert_eq!(
        all_flags().into_iter().filter(|f| conflicting(*f)).count(),
        76
    );
    let dry = EffectGate::new(
        EffectFlags {
            dry_run: true,
            ..Default::default()
        },
        Scope::Rank,
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(dry.receipt()).unwrap(),
        json!({
            "response_cache": {"state": "disabled", "by": "dry-run"},
            "ledger": {"state": "disabled", "by": "dry-run"},
            "runtime_state": {"state": "disabled", "by": "dry-run"},
            "network": {"state": "blocked", "by": "dry-run"},
            "unverified_children": false,
            "cross_process_coordination": false,
            "stateless": true
        })
    );
}

#[test]
fn the_response_cache_touches_disk_only_when_the_gate_permits() {
    let mut opened = 0;
    for (flags, gate) in gates() {
        let root = private_tree("cache");
        let location = root.join("cache");
        let run = Run::new();
        let result = gate.open_cache(
            &run.invocation,
            &run.cx,
            CacheAccess::Initialize,
            CacheLocation::Directory(location.clone()),
        );
        match gate.response_cache() {
            StoreAccess::Enabled => {
                assert!(matches!(result, Ok(CacheOpen::Ready(_))), "{flags:?}");
                assert!(location.join(CACHE_FILE).is_file(), "positive twin");
                opened += 1;
            }
            StoreAccess::Disabled(_) => {
                assert!(matches!(result, Ok(CacheOpen::Disabled)), "{flags:?}");
                assert_eq!(entries(&root), 0, "{flags:?} created state");
                // A disabled cache never resolves its location: an unsafe
                // relative path is not even examined.
                let unsafe_location = gate.open_cache(
                    &run.invocation,
                    &run.cx,
                    CacheAccess::Initialize,
                    CacheLocation::Directory(PathBuf::from("relative/cache")),
                );
                assert!(matches!(unsafe_location, Ok(CacheOpen::Disabled)));
            }
        }
        drop(result);
        run.finish();
    }
    // Enabled: neither dry-run, no-persist nor no-cache.
    assert!(opened > 0);
    let enabled = gates()
        .iter()
        .filter(|(f, _)| !(f.dry_run || f.no_persist || f.no_cache))
        .count();
    assert_eq!(opened, enabled);
}

fn caps() -> serde_json::Value {
    json!({"crate_version":"0.8.0","api_version":1,"contract_version":"1","build_commit":"unknown","global_flags":[{"name":"db"}],"features":["json_output","export_command","self_describing_capabilities"],"commands":[{"name":"export","arguments":[{"name":"path"},{"name":"source"},{"name":"format","enum_values":["json"]},{"name":"include-tools"}]},{"name":"sessions","arguments":[{"name":"workspace"},{"name":"limit"},{"name":"json"}]}]})
}

/// A qualified-looking cass that records every start in `marker`.
fn fake_cass(root: &Path, marker: &Path) -> CassConfig {
    let path = root.join("cass-fixture");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    writeln!(
        file,
        "#!/bin/sh\nprintf x >> '{}'\nif [ \"$3\" = capabilities ]; then\n/bin/cat <<'CAPS'\n{}\nCAPS\nfi",
        marker.display(),
        caps()
    )
    .unwrap();
    file.set_permissions(fs::Permissions::from_mode(0o700))
        .unwrap();
    drop(file);
    CassConfig {
        executable: TrustedExecutable::resolve(&path, &[root.to_path_buf()]).unwrap(),
        directory: root.to_path_buf(),
        database: root.join("explicit.db"),
        binary_digest: ContentHash::from_bytes(&fs::read(&path).unwrap()),
    }
}

#[test]
fn unverified_children_start_only_when_the_gate_permits() {
    let mut started = 0;
    for (flags, gate) in gates() {
        let root = private_tree("cass");
        let marker = root.join("started");
        let config = fake_cass(&root, &marker);
        let run = Run::new();
        let result = run.invocation.runtime().block_on(CassAdapter::probe(
            config,
            gate.source_policy(),
            &run.cx,
            &run.clock,
        ));
        if gate.unverified_children() {
            assert!(result.is_ok(), "{flags:?}: {:?}", result.err());
            assert!(marker.exists(), "positive twin: the child started");
            started += 1;
        } else {
            assert_eq!(
                result.err(),
                Some(CassError::UnsupportedSourceMode),
                "{flags:?}"
            );
            assert!(!marker.exists(), "{flags:?} started cass");
        }
        run.finish();

        // An explicitly selected cass session: refused with exit 7 before any
        // read, never silently replaced by another source.
        let options = SourceOptions {
            cass_session: Some(LocalPath::new(PathBuf::from("/synthetic/session.jsonl"))),
            ..Default::default()
        };
        let workspace = WorkspaceId::new("w_effects").unwrap();
        let selected = options.resolve(workspace, gate.source_policy(), false, |_, _| {
            panic!("an explicit source never runs discovery")
        });
        let source = gate.source_policy();
        match selected {
            Ok(SelectionOutcome::Selected(selection)) => {
                assert!(gate.unverified_children(), "{flags:?}");
                assert!(matches!(selection.target(), SourceTarget::CassSession(_)));
            }
            Err(SourceError::CassUnavailableInMode) => {
                assert!(!gate.unverified_children(), "{flags:?}");
                assert_eq!(
                    SourceError::CassUnavailableInMode.kind(),
                    ErrorKind::UnsupportedSourceMode
                );
                assert_eq!(ErrorKind::UnsupportedSourceMode.exit_code(), CliExit::Input);
                assert_eq!(CliExit::Input as u8, 7);
            }
            Err(SourceError::ConflictingFlags) => {
                // --allow-network has no meaning for local inspection.
                assert!(source.local_only && source.allow_network, "{flags:?}");
            }
            other => panic!("{flags:?}: unexpected {other:?}"),
        }
    }
    assert_eq!(
        started,
        gates()
            .iter()
            .filter(|(f, g)| !(f.offline || f.dry_run || g.scope() == Scope::LocalInspection))
            .count()
    );
}

fn request() -> Request {
    Request::new(
        "jev-latest".into(),
        json!("synthetic effect gate probe"),
        [(
            "fit".into(),
            Question::Noul {
                instructions: json!("Return how strongly this synthetic example calls for help."),
                criteria: None,
            },
        )],
    )
    .unwrap()
}

fn trusted_config(network_enabled: bool) -> ResolvedConfig {
    ResolvedConfig::resolve(
        ConfigSources {
            trusted_user: vec![("network.enabled".into(), RawValue::Bool(network_enabled))],
            environment: vec![(OsString::from("TYPESAFE_API_KEY"), OsString::from(CANARY))],
            ..Default::default()
        },
        1,
    )
    .unwrap()
}

/// Accepts and immediately closes TCP connections, counting each one.
struct Acceptor {
    port: u16,
    accepted: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Acceptor {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (count, done) = (accepted.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        count.fetch_add(1, Ordering::SeqCst);
                        drop(stream);
                    }
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(2)),
                }
            }
        });
        Self {
            port,
            accepted,
            stop,
            thread: Some(thread),
        }
    }
    fn count(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }
}

impl Drop for Acceptor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[test]
fn provider_sockets_open_only_when_the_gate_permits() {
    let acceptor = Acceptor::new();
    let endpoint =
        EndpointConfig::from_base_origin_str(&format!("https://localhost:{}", acceptor.port))
            .unwrap();
    let client = JevClient::new(endpoint).unwrap();
    let mut attempts = 0;
    for (flags, gate) in gates() {
        for network_enabled in [false, true] {
            let config = trusted_config(network_enabled);
            let consent = gate.network_consent(&config);
            let expected = match (gate.scope(), flags.offline, flags.dry_run) {
                (Scope::LocalInspection, _, _) | (Scope::Rank, true, _) => {
                    NetworkConsent::Blocked(NetworkBlock::Offline)
                }
                (Scope::Rank, false, true) => NetworkConsent::Blocked(NetworkBlock::DryRun),
                _ if flags.allow_network => {
                    NetworkConsent::Authorized(ConsentSource::AllowNetworkFlag)
                }
                _ if network_enabled => {
                    NetworkConsent::Authorized(ConsentSource::TrustedUserConfig)
                }
                _ => NetworkConsent::NotAuthorized,
            };
            assert_eq!(
                consent, expected,
                "{flags:?} network.enabled={network_enabled}"
            );
            let key =
                OriginScopedCredential::bind(config.credential().unwrap().clone(), client.origin())
                    .unwrap();
            let before = acceptor.count();
            let run = Run::new();
            let result = run.invocation.runtime().block_on(client.send(
                &request(),
                Some(&key),
                consent,
                &run.cx,
                &run.clock,
            ));
            run.finish();
            let Err(error) = result else {
                panic!("the acceptor never answers TLS");
            };
            assert!(!format!("{error} {error:?}").contains(CANARY));
            match consent {
                NetworkConsent::Authorized(_) => {
                    assert!(error.http_attempt_started, "{flags:?}");
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                    while acceptor.count() == before && std::time::Instant::now() < deadline {
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                    assert!(acceptor.count() > before, "positive twin: a socket opened");
                    attempts += 1;
                }
                NetworkConsent::Blocked(_) | NetworkConsent::NotAuthorized => {
                    let refusal = match consent {
                        NetworkConsent::Blocked(NetworkBlock::Offline) => {
                            ProviderAdmissionRefusal::Offline
                        }
                        NetworkConsent::Blocked(NetworkBlock::DryRun) => {
                            ProviderAdmissionRefusal::DryRun
                        }
                        _ => ProviderAdmissionRefusal::NetworkNotAuthorized,
                    };
                    assert_eq!(
                        error.kind,
                        TransportErrorKind::Admission(refusal),
                        "{flags:?}"
                    );
                    assert!(!error.http_attempt_started);
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    assert_eq!(acceptor.count(), before, "{flags:?} opened a socket");
                    // Offline with no cached result is a cache miss (exit 11);
                    // other refusals are privacy denials (exit 8).
                    let exit = refusal.kind().exit_code() as u8;
                    assert_eq!(
                        exit,
                        if refusal == ProviderAdmissionRefusal::Offline {
                            11
                        } else {
                            8
                        }
                    );
                }
            }
        }
    }
    assert!(attempts > 0);
}

#[test]
fn configuration_and_explicit_requests_are_read_in_every_mode() {
    let root = private_tree("inputs");
    let workspace = root.join("workspace");
    let user = root.join("user");
    fs::create_dir_all(workspace.join(".claude/skills/alpha")).unwrap();
    fs::create_dir_all(user.join("sr")).unwrap();
    fs::write(user.join("sr/config.toml"), "[ranking]\ntop=3\n").unwrap();
    fs::write(
        workspace.join(".claude/skills/alpha/SKILL.md"),
        "---\nname: alpha\ndescription: Alpha skill.\n---\n# alpha\n\nStatic guidance.\n",
    )
    .unwrap();
    let state = root.join("state");
    DirBuilder::new().mode(0o700).create(&state).unwrap();
    let visibility = Visibility::Verified {
        contract_version: "test-adapter-v1".into(),
    };
    for (flags, _gate) in gates() {
        let run = Run::new();
        // Ordinary configuration is not a state store: it is read in every mode.
        let files = ConfigFiles::new(workspace.clone(), Some(user.clone()));
        let config = files.load(&run.clock, ConfigSources::default()).unwrap();
        assert_eq!(config.effective().top(), 3, "{flags:?}");
        // Explicit requests resolve locally, with no store, child or socket.
        let plan = claude_code_plan(&workspace, None, visibility.clone()).unwrap();
        let roster = resolve_claude_plan(
            &plan,
            &std::collections::BTreeMap::new(),
            &run.cx,
            &run.clock,
        )
        .unwrap();
        let explicit = resolve_explicit_requirements(
            &ExplicitResolutionRequest {
                cli_required_skills: vec!["alpha".into()],
                ..Default::default()
            },
            &roster,
        )
        .unwrap();
        assert!(
            matches!(explicit, ExplicitResolutionResult::Resolved { ref skills, .. } if skills.len() == 1),
            "{flags:?}: {explicit:?}"
        );
        run.finish();
    }
    assert_eq!(entries(&state), 0, "input reads create no state");
}
