//! Cache and coordination identity bind where a context came from, who
//! produced it, which agent it belongs to and which native branch is active.
//! Identical redacted requests from different producers, agents, forks or
//! source kinds must never share a cached answer or a single-flight lease.
use asupersync::Cx;
use serde_json::{Value, json};
use skillranker::cache::{
    CacheKey, CacheNamespace, CoordinationKey, MemoryResponseCache, RequestFingerprint, SourceKind,
};
use skillranker::config::ConfigSources;
use skillranker::context::source::SourceOptions;
use skillranker::effects::{EffectGate, Scope};
use skillranker::identity::{
    AdapterId, AdapterVersion, AgentId, EventId, HarnessId, ProducerId, SessionId, WorkspaceId,
};
use skillranker::jev::OriginScopedCredential;
use skillranker::jev::client::{TransportError, TransportErrorKind, TransportFuture};
use skillranker::jev::codec::{Question, Request};
use skillranker::jev::retry::RetryAfter;
use skillranker::limits::DurationMillis;
use skillranker::pipeline::{JevTransport, RankArgs, execute_pipeline};
use skillranker::privacy::{EffectFlags, NetworkConsent};
use skillranker::roster::LocalPath;
use skillranker::runtime::{EntryClock, ProcessInvocation};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// A shared baseline: same harness, workspace and session in every variant.
fn base() -> CacheNamespace {
    CacheNamespace::new(HarnessId::new("claude_code").unwrap(), 1)
        .with_workspace(WorkspaceId::new("/workspace").unwrap())
        .with_session(SessionId::new("session-1").unwrap())
}

fn identity(key: &CacheKey, namespace: &CacheNamespace) -> ([u8; 32], CoordinationKey) {
    let request = RequestFingerprint::from_bytes([7; 32]);
    (
        MemoryResponseCache::namespace_hash(key, namespace),
        CoordinationKey::compute(key, namespace, &request),
    )
}

#[test]
fn each_identity_dimension_separates_cache_and_lease_namespaces() {
    let key = CacheKey::generate().unwrap();
    let native = || {
        base().with_source_kind(SourceKind::Native).with_adapter(
            AdapterId::new("claude_code").unwrap(),
            AdapterVersion::new("1").unwrap(),
        )
    };
    let normalized = || {
        base()
            .with_source_kind(SourceKind::Normalized)
            .with_producer(ProducerId::new("producer-a").unwrap())
            .with_agent(AgentId::new("agent-a").unwrap())
    };
    let variants = [
        // A normalized import claiming a native session's IDs stays apart.
        (
            "source kind",
            native(),
            base().with_source_kind(SourceKind::Normalized),
        ),
        (
            "producer",
            normalized(),
            normalized().with_producer(ProducerId::new("producer-b").unwrap()),
        ),
        (
            "agent",
            normalized(),
            normalized().with_agent(AgentId::new("agent-b").unwrap()),
        ),
        (
            "native branch leaf",
            native().with_leaf_event(EventId::new("leaf-a").unwrap()),
            native().with_leaf_event(EventId::new("leaf-b").unwrap()),
        ),
        (
            "absent versus present producer",
            base().with_source_kind(SourceKind::Normalized),
            base()
                .with_source_kind(SourceKind::Normalized)
                .with_producer(ProducerId::new("producer-a").unwrap()),
        ),
    ];
    for (dimension, left, right) in variants {
        let (left_ns, left_lease) = identity(&key, &left);
        let (right_ns, right_lease) = identity(&key, &right);
        assert_ne!(left_ns, right_ns, "{dimension}: cache namespace");
        assert_ne!(left_lease, right_lease, "{dimension}: lease key");
    }
}

#[test]
fn the_same_identity_keeps_one_namespace() {
    let key = CacheKey::generate().unwrap();
    let make = || {
        base()
            .with_source_kind(SourceKind::Normalized)
            .with_producer(ProducerId::new("producer-a").unwrap())
            .with_agent(AgentId::new("agent-a").unwrap())
            .with_leaf_event(EventId::new("leaf-a").unwrap())
    };
    assert_eq!(identity(&key, &make()), identity(&key, &make()));
}

#[test]
fn adjacent_identity_fields_do_not_run_together() {
    // Framing keeps a value from moving between neighbouring fields.
    let key = CacheKey::generate().unwrap();
    let left = base()
        .with_producer(ProducerId::new("ab").unwrap())
        .with_agent(AgentId::new("c").unwrap());
    let right = base()
        .with_producer(ProducerId::new("a").unwrap())
        .with_agent(AgentId::new("bc").unwrap());
    assert_ne!(identity(&key, &left).0, identity(&key, &right).0);
    let producer = base().with_producer(ProducerId::new("same").unwrap());
    let agent = base().with_agent(AgentId::new("same").unwrap());
    assert_ne!(identity(&key, &producer).0, identity(&key, &agent).0);
}

// ---------------------------------------------------------------------------
// Through the real rank pipeline and persistent cache.

static NEXT: AtomicU64 = AtomicU64::new(0);
const REQUEST: &str = "The rust tests are failing; find and repair the failing test.";

/// Answers every question of the request it receives: each noul 0.9, each
/// Choice favoring its first real option. Counts what it served.
#[derive(Default)]
struct CountingProvider {
    served: AtomicUsize,
}

impl JevTransport for CountingProvider {
    fn send<'a>(
        &'a self,
        request: &'a Request,
        _credential: Option<&'a OriginScopedCredential>,
        _consent: NetworkConsent,
        _cx: &'a Cx,
        _clock: &'a EntryClock,
    ) -> TransportFuture<'a> {
        self.served.fetch_add(1, Ordering::SeqCst);
        let mut answers = serde_json::Map::new();
        for (id, question) in request.questions() {
            let answer = match question {
                Question::Noul { .. } => json!({"type": "noul", "noul": 0.9}),
                Question::Choice { criteria, .. } => {
                    let favored = criteria
                        .keys()
                        .find(|key| key.as_str() != "__none__")
                        .unwrap_or_else(|| criteria.keys().next().unwrap())
                        .clone();
                    let rest = 0.2 / (criteria.len().max(2) - 1) as f64;
                    let probabilities: serde_json::Map<String, Value> = criteria
                        .keys()
                        .map(|key| (key.clone(), json!(if *key == favored { 0.8 } else { rest })))
                        .collect();
                    json!({"type": "choice", "choice": favored,
                           "probabilities": probabilities, "confidence": 0.8})
                }
            };
            answers.insert(id.clone(), answer);
        }
        let body = serde_json::to_vec(&json!({"model": "jev-test", "answers": answers,
            "usage": {"input_tokens": 10, "output_tokens": 5}}))
        .unwrap();
        let result = request
            .decode_response(&body)
            .map_err(|error| TransportError {
                kind: TransportErrorKind::Response(error),
                http_attempt_started: true,
                retry_after: RetryAfter::Absent,
            });
        Box::pin(async move { result })
    }
}

// Intentionally retained: repository policy forbids automatic tree deletion.
struct Workspace {
    root: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "sr-cache-identity-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        for name in ["alpha", "beta"] {
            let dir = root.join("workspace/.claude/skills").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {name} repairs failing rust tests.\n---\nBody.\n"),
            )
            .unwrap();
        }
        std::fs::create_dir_all(root.join("config/sr")).unwrap();
        std::fs::write(
            root.join("config/sr/config.toml"),
            "[network]\nenabled = true\n",
        )
        .unwrap();
        // A private cache directory under root-owned sticky /tmp.
        let cache = Path::new("/tmp").join(format!(
            "sr-cache-identity-cache-{}",
            root.file_name().unwrap().to_string_lossy()
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&cache)
            .unwrap();
        Self { root }
    }
    fn workspace(&self) -> PathBuf {
        std::fs::canonicalize(self.root.join("workspace")).unwrap()
    }
    fn cache(&self) -> PathBuf {
        Path::new("/tmp").join(format!(
            "sr-cache-identity-cache-{}",
            self.root.file_name().unwrap().to_string_lossy()
        ))
    }
    /// Normalized context for one session, with the given producer and agent.
    fn normalized(&self, name: &str, producer: &str, agent: &str) -> SourceOptions {
        let path = self.root.join(format!("{name}.json"));
        let context = json!({
            "schema_version": 1, "harness": "claude_code", "producer_id": producer,
            "workspace_root": self.workspace().to_string_lossy(), "session_id": "session-1",
            "agent_id": agent, "branch_id": null, "context_epoch": null,
            "current_request": {"event_id": "request-1", "text": REQUEST,
                                "attachments_omitted": false, "essential_attachment_missing": false},
            "events": [{"event_id": "request-1", "parent_id": null, "turn_id": "turn-1",
                        "agent_id": agent, "branch_id": null, "role": "user", "kind": "message",
                        "timestamp_unix_ms": null, "text": REQUEST, "tool": null}],
            "explicit_skill_references": [], "supplied_loads": []
        });
        std::fs::write(&path, serde_json::to_vec(&context).unwrap()).unwrap();
        SourceOptions {
            context: Some(LocalPath::new(path)),
            ..Default::default()
        }
    }
    /// A Claude transcript of session `s` in its own directory, whose latest
    /// request has native identity `leaf`. Only the leaf differs between forks.
    fn native(&self, dir: &str, leaf: &str) -> SourceOptions {
        let directory = self.root.join(dir);
        std::fs::create_dir_all(&directory).unwrap();
        let workspace = self.workspace();
        let path = directory.join("s.jsonl");
        let record = json!({"type": "user", "uuid": leaf, "parentUuid": null,
                            "cwd": workspace, "sessionId": "s",
                            "message": {"role": "user", "content": REQUEST}});
        std::fs::write(&path, record.to_string() + "\n").unwrap();
        SourceOptions {
            transcript: Some(LocalPath::new(path)),
            harness: Some(HarnessId::new("claude_code").unwrap()),
            ..Default::default()
        }
    }
    /// Rank with the persistent cache; returns whether it was a cache hit.
    fn rank(&self, provider: &CountingProvider, source: SourceOptions) -> bool {
        let flags = EffectFlags {
            offline: false,
            allow_network: false,
            dry_run: false,
            no_cache: false,
            no_ledger: true,
            no_persist: false,
            save_case: false,
        };
        let args = RankArgs {
            workspace: self.workspace(),
            user_config_root: Some(self.root.join("config")),
            home: None,
            cache_dir: Some(self.cache()),
            ledger_dir: None,
            sources: ConfigSources {
                environment: vec![(
                    std::ffi::OsString::from("TYPESAFE_API_KEY"),
                    std::ffi::OsString::from("synthetic-identity-canary"),
                )],
                ..Default::default()
            },
            gate: EffectGate::new(flags, Scope::Rank).unwrap(),
            source_options: source,
            require_skills: Vec::new(),
            shortlist_ids: Vec::new(),
            roster_file: None,
            explain: false,
            why_not: None,
            cursor: None,
            output_json: true,
            output_table: false,
            dry_run: false,
            save_case: None,
        };
        let clock = EntryClock::capture_with(
            DurationMillis::new("identity-total", 10_000, 30_000).unwrap(),
            DurationMillis::new("identity-cleanup", 200, 30_000).unwrap(),
        )
        .unwrap();
        let invocation = ProcessInvocation::from_clock(clock).unwrap();
        let cx = invocation.request_cx().unwrap();
        let document = invocation
            .runtime()
            .block_on(async { execute_pipeline(&invocation, &cx, args, Some(provider)).await })
            .expect("ranked");
        assert!(invocation.shutdown(), "owned runtime must shut down");
        let value = document.as_value().clone();
        assert_eq!(value["decision"], "ranked", "{value}");
        value["cache"]["hit"] == true
    }
}

#[test]
fn different_producers_or_agents_never_share_a_cached_answer() {
    for (dimension, second) in [
        ("producer", ("producer-b", "agent-a")),
        ("agent", ("producer-a", "agent-b")),
    ] {
        let w = Workspace::new();
        let provider = CountingProvider::default();
        assert!(!w.rank(&provider, w.normalized("first", "producer-a", "agent-a")));
        // Honest twin: the exact same identity is served from the cache.
        assert!(
            w.rank(&provider, w.normalized("repeat", "producer-a", "agent-a")),
            "{dimension}"
        );
        assert_eq!(provider.served.load(Ordering::SeqCst), 2, "{dimension}");
        let (producer, agent) = second;
        assert!(
            !w.rank(&provider, w.normalized("other", producer, agent)),
            "{dimension}"
        );
        assert_eq!(provider.served.load(Ordering::SeqCst), 4, "{dimension}");
    }
}

#[test]
fn native_forks_of_one_session_never_share_a_cached_answer() {
    let w = Workspace::new();
    let provider = CountingProvider::default();
    assert!(!w.rank(&provider, w.native("fork-a", "leaf-a")));
    assert!(
        w.rank(&provider, w.native("fork-a", "leaf-a")),
        "exact repeat"
    );
    assert_eq!(provider.served.load(Ordering::SeqCst), 2);
    // Same session ID and request bytes, a different active leaf.
    assert!(!w.rank(&provider, w.native("fork-b", "leaf-b")));
    assert_eq!(provider.served.load(Ordering::SeqCst), 4);
}
