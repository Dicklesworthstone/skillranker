//! Namespace operations must remain isolated even when raw identity bytes concatenate equally.
use skillranker::cache::{
    CacheKey, CacheLookupQuery, CacheLookupResult, CacheNamespace, CachedResponseEntry,
    CoordinationKey, MemoryResponseCache, RequestFingerprint, RequestFingerprintInput,
    RequestStage, compute_request_fingerprint,
};
use skillranker::identity::{BranchId, HarnessId, SessionId, WorkspaceId};
use skillranker::jev::codec::Usage;

fn namespace() -> CacheNamespace {
    CacheNamespace::new(HarnessId::new("claude_code").unwrap(), 1)
}

fn collisions() -> Vec<(CacheNamespace, CacheNamespace)> {
    vec![
        (
            namespace()
                .with_workspace(WorkspaceId::new("ab").unwrap())
                .with_session(SessionId::new("c").unwrap()),
            namespace()
                .with_workspace(WorkspaceId::new("a").unwrap())
                .with_session(SessionId::new("bc").unwrap()),
        ),
        (
            namespace().with_workspace(WorkspaceId::new("same").unwrap()),
            namespace().with_session(SessionId::new("same").unwrap()),
        ),
        (
            namespace().with_session(SessionId::new("same").unwrap()),
            namespace().with_branch(BranchId::new("same").unwrap()),
        ),
    ]
}

fn fingerprint(key: &CacheKey, ns: &CacheNamespace) -> RequestFingerprint {
    compute_request_fingerprint(
        key,
        ns,
        &RequestFingerprintInput {
            stage: RequestStage::Wide,
            canonical_redacted_state: b"synthetic context",
            candidates: &[],
            questions_digest: [0; 32],
            endpoint_url: "https://api.typesafe.ai",
            model: "model",
            prompt_version: "1",
            adapter_version: "1",
            privacy_policy_version: "1",
            excerpt_strategy: "default",
        },
    )
}

fn entry(fp: RequestFingerprint) -> CachedResponseEntry {
    CachedResponseEntry {
        stage: RequestStage::Wide,
        request_fingerprint: fp,
        response_bytes: b"synthetic cache payload".to_vec(),
        received_at_unix_ms: 1000,
        ttl_seconds: 600,
        model: "model".into(),
        model_revision: None,
        original_usage: Usage {
            input_tokens: 1,
            output_tokens: 1,
        },
        attempt_id: None,
    }
}

fn lookup(
    cache: &MemoryResponseCache,
    key: &CacheKey,
    ns: &CacheNamespace,
    fp: &RequestFingerprint,
) -> CacheLookupResult {
    cache
        .get(&CacheLookupQuery {
            key,
            namespace: ns,
            stage: RequestStage::Wide,
            fingerprint: fp,
            now_unix_ms: 1001,
            active_model: "model",
            active_revision: None,
        })
        .unwrap()
}

#[test]
fn eviction_preserves_distinct_namespaces_with_real_request_fingerprints() {
    let key = CacheKey::from_bytes([4; 32]);
    for (first, second) in collisions() {
        let cache = MemoryResponseCache::new();
        let a = fingerprint(&key, &first);
        let b = fingerprint(&key, &second);
        assert_ne!(
            a, b,
            "request fingerprint already frames namespace identities"
        );
        cache.put(&key, &first, entry(a)).unwrap();
        cache.put(&key, &second, entry(b)).unwrap();
        assert!(matches!(
            lookup(&cache, &key, &first, &a),
            CacheLookupResult::Hit { .. }
        ));
        assert!(matches!(
            lookup(&cache, &key, &second, &b),
            CacheLookupResult::Hit { .. }
        ));
        assert_eq!(
            cache.evict_namespace(&key, &first).unwrap(),
            1,
            "eviction must remove only the requested namespace"
        );
        assert!(matches!(
            lookup(&cache, &key, &first, &a),
            CacheLookupResult::Miss
        ));
        assert!(matches!(
            lookup(&cache, &key, &second, &b),
            CacheLookupResult::Hit { .. }
        ));
        assert_eq!(cache.evict_namespace(&key, &second).unwrap(), 1);
        assert_eq!(cache.evict_namespace(&key, &second).unwrap(), 0);
    }
}

#[test]
fn coordination_binds_field_boundaries_even_for_the_same_request_handle() {
    let key = CacheKey::from_bytes([4; 32]);
    let request = RequestFingerprint::from_bytes([7; 32]);
    for (first, second) in collisions() {
        let a = CoordinationKey::compute(&key, &first, &request);
        assert_eq!(a, CoordinationKey::compute(&key, &first.clone(), &request));
        assert_ne!(a, CoordinationKey::compute(&key, &second, &request));
    }
}

#[test]
fn cache_lookup_cannot_cross_namespaces_with_a_reused_request_handle() {
    let key = CacheKey::from_bytes([4; 32]);
    let request = RequestFingerprint::from_bytes([7; 32]);
    for (first, second) in collisions() {
        let cache = MemoryResponseCache::new();
        cache.put(&key, &first, entry(request)).unwrap();
        assert!(matches!(
            lookup(&cache, &key, &first, &request),
            CacheLookupResult::Hit { .. }
        ));
        assert!(matches!(
            lookup(&cache, &key, &second, &request),
            CacheLookupResult::Miss
        ));
    }
}
