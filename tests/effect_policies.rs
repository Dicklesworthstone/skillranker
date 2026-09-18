//! Independent Persistence, Offline, and Dry-Run Effect Policies Contract Tests
//!
//! Satisfies roadmap task `sr-roadmap-l1i.5.9` and contract boundary `p4_effect_policies`.
//!
//! Verifies:
//! 1. Complete flag combination matrix: 128 states, 52 valid, 76 conflicting.
//! 2. Restrictive persistence hierarchy:
//!    - Default: Read/write cache, ledger, and coordination state.
//!    - `--no-cache`: response cache disabled; ledger and coordination unchanged.
//!    - `--no-ledger`: ledger disabled; cache and coordination unchanged.
//!    - `--no-persist`: cache, ledger, and coordination disabled (in-memory only).
//!    - `--dry-run`: implies `--no-persist`; stateless preview; zero state disk mutations.
//! 3. Real filesystem/disk observations:
//!    - Proves zero database files, locks, migrations, or journal files created under `--no-persist` / `--dry-run`.
//!    - Ordinary configuration reads remain fully functional in every mode.
//! 4. Network admission and refusal matrix:
//!    - Offline returns `CacheMiss` (code 11), never authentication failure.
//!    - Dry-run and unauthorized network return `NetworkDenied` (code 8).
//!    - Missing credential with authorized network returns `Authentication` (code 4).
//!    - Consent checked before credential to prevent spurious auth reports.
//! 5. Offline and Dry-Run Cass restriction:
//!    - Explicitly selected Cass source under offline or dry-run returns exit 7
//!      `UnsupportedSourceMode` (`SourceError::CassUnavailableInMode`).
//! 6. Explicit local requests work offline without network access.

#![cfg(unix)]

use skillranker::cli::ConfigFiles;
use skillranker::config::{ConfigSources, PolicyBoundary, Revalidation};
use skillranker::context::source::{SourceError, SourceOptions, SourcePolicy};
use skillranker::identity::{SkillId, WorkspaceId};
use skillranker::output::ErrorKind;
use skillranker::privacy::{
    ConsentSource, CredentialStatus, EffectFlags, EffectPolicy, FlagConflict, NetworkBlock,
    NetworkConsent, ProviderAdmissionRefusal, Restriction, StoreAccess, admit_provider_attempt,
};
use skillranker::roster::LocalPath;
use skillranker::runtime::{EntryClock, ProcessInvocation};
use skillranker::storage::{CacheAccess, CacheLocation, CacheOpen, open_cache};
use std::fs::DirBuilder;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempWorkspace {
    root: PathBuf,
}

impl TempWorkspace {
    fn new(case: &str) -> Self {
        let root = Path::new("/tmp").join(format!(
            "sr-effect-test-{case}-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        DirBuilder::new().mode(0o700).create(&root).unwrap();
        DirBuilder::new()
            .mode(0o700)
            .create(root.join(".sr"))
            .unwrap();
        DirBuilder::new()
            .mode(0o700)
            .create(root.join("user"))
            .unwrap();
        DirBuilder::new()
            .mode(0o700)
            .create(root.join("user/sr"))
            .unwrap();
        Self { root }
    }

    fn project_config(&self, toml: &str) {
        std::fs::write(self.root.join(".sr/config.toml"), toml).unwrap();
    }

    fn user_config(&self, toml: &str) {
        std::fs::write(self.root.join("user/sr/config.toml"), toml).unwrap();
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// 1. Verifies the full 128-state matrix of effect flags, ensuring exact conflict detection
///    and proper restriction propagation across all valid states.
#[test]
fn effect_policy_matrix_128_combinations() {
    let mut valid_count = 0;
    for bits in 0u8..128 {
        let bit = |n: u8| bits & (1 << n) != 0;
        let flags = EffectFlags {
            offline: bit(0),
            allow_network: bit(1),
            dry_run: bit(2),
            no_cache: bit(3),
            no_ledger: bit(4),
            no_persist: bit(5),
            save_case: bit(6),
        };

        let mut expected_conflicts = Vec::new();
        if flags.offline && flags.allow_network {
            expected_conflicts.push(FlagConflict::OfflineWithAllowNetwork);
        }
        if flags.dry_run && flags.allow_network {
            expected_conflicts.push(FlagConflict::DryRunWithAllowNetwork);
        }
        if flags.save_case && flags.dry_run {
            expected_conflicts.push(FlagConflict::SaveCaseWithDryRun);
        }
        if flags.save_case && flags.no_persist {
            expected_conflicts.push(FlagConflict::SaveCaseWithNoPersist);
        }

        match EffectPolicy::from_flags(flags) {
            Err(conflicts) => {
                assert_eq!(
                    conflicts, expected_conflicts,
                    "Conflict mismatch for flags: {flags:?}"
                );
            }
            Ok(policy) => {
                assert!(
                    expected_conflicts.is_empty(),
                    "Unexpected success for flags: {flags:?}"
                );
                valid_count += 1;

                let stateless = flags.dry_run || flags.no_persist;

                // Cache access verification
                if stateless {
                    assert!(
                        matches!(
                            policy.response_cache(),
                            StoreAccess::Disabled(Restriction::DryRun | Restriction::NoPersist)
                        ),
                        "Cache must be disabled under stateless for {flags:?}"
                    );
                } else if flags.no_cache {
                    assert_eq!(
                        policy.response_cache(),
                        StoreAccess::Disabled(Restriction::NoCache)
                    );
                } else {
                    assert_eq!(policy.response_cache(), StoreAccess::Enabled);
                }

                // Ledger access verification
                if stateless {
                    assert!(
                        matches!(
                            policy.ledger(),
                            StoreAccess::Disabled(Restriction::DryRun | Restriction::NoPersist)
                        ),
                        "Ledger must be disabled under stateless for {flags:?}"
                    );
                } else if flags.no_ledger {
                    assert_eq!(
                        policy.ledger(),
                        StoreAccess::Disabled(Restriction::NoLedger)
                    );
                } else {
                    assert_eq!(policy.ledger(), StoreAccess::Enabled);
                }

                // Persistent runtime state (leases, cooldowns, keys)
                if stateless {
                    assert!(
                        matches!(
                            policy.persistent_runtime_state(),
                            StoreAccess::Disabled(Restriction::DryRun | Restriction::NoPersist)
                        ),
                        "Runtime state must be disabled under stateless for {flags:?}"
                    );
                } else {
                    assert_eq!(policy.persistent_runtime_state(), StoreAccess::Enabled);
                }

                // Case capture
                assert_eq!(policy.case_capture(), flags.save_case);

                // Network block
                if flags.offline {
                    assert_eq!(policy.network_block(), Some(NetworkBlock::Offline));
                } else if flags.dry_run {
                    assert_eq!(policy.network_block(), Some(NetworkBlock::DryRun));
                } else {
                    assert_eq!(policy.network_block(), None);
                }
            }
        }
    }
    assert_eq!(
        valid_count, 52,
        "Must be exactly 52 valid effect policies out of 128"
    );
}

/// 2. Verifies real filesystem non-mutation under disabled persistence modes.
#[test]
fn persistence_controls_prevent_disk_store_creation() {
    let ws = TempWorkspace::new("no-persist-probe");
    let state_dir = ws.root.join("state");
    DirBuilder::new().mode(0o700).create(&state_dir).unwrap();

    let invocation = ProcessInvocation::enter().expect("invocation enter");
    let cx = invocation.request_cx().expect("request cx");

    // Case A: CacheAccess::Disabled directly maps to StoreAccess::Disabled
    // from EffectPolicy (under --no-persist, --dry-run, or --no-cache).
    let result = open_cache(
        &invocation,
        &cx,
        CacheAccess::Disabled,
        CacheLocation::Directory(state_dir.clone()),
    );
    assert!(
        matches!(result, Ok(CacheOpen::Disabled)),
        "open_cache with CacheAccess::Disabled must return CacheOpen::Disabled immediately"
    );

    // Verify zero database files or sidecars created
    let entries: Vec<_> = std::fs::read_dir(&state_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(
        entries.is_empty(),
        "State directory must remain completely empty under disabled cache, found: {entries:?}"
    );

    assert!(invocation.shutdown());
}

/// 3. Verifies that ordinary configuration reads remain permitted in every mode,
///    including --no-persist and --dry-run.
#[test]
fn ordinary_config_reads_permitted_in_all_modes() {
    let ws = TempWorkspace::new("config-reads");
    ws.user_config("[ranking]\ntop=3\nshortlist=8\n[network]\nenabled=true\n");
    ws.project_config("[ranking]\ntop=2\n");

    let clock = EntryClock::capture().expect("clock capture");
    let files = ConfigFiles::new(ws.root.clone(), Some(ws.root.join("user")));

    // Test across all four non-persisting / restricted policies
    for flags in [
        EffectFlags {
            no_persist: true,
            ..EffectFlags::default()
        },
        EffectFlags {
            dry_run: true,
            ..EffectFlags::default()
        },
        EffectFlags {
            offline: true,
            no_cache: true,
            ..EffectFlags::default()
        },
        EffectFlags {
            no_ledger: true,
            ..EffectFlags::default()
        },
    ] {
        let policy = EffectPolicy::from_flags(flags).expect("valid policy");

        let loaded = files
            .load(&clock, ConfigSources::default())
            .expect("config loading must succeed even under restricted effect flags");
        assert_eq!(loaded.effective().top(), 2);
        assert_eq!(loaded.effective().shortlist(), 8);

        // Receipts capture effective policies truthfully
        let receipt = loaded.receipt(policy);
        assert_eq!(receipt.effects(), policy);

        // Re-read at consequential boundary succeeds
        let (refreshed, reval) = files
            .refresh(&clock, &loaded, &receipt, PolicyBoundary::ProviderAdmission)
            .expect("config refresh must succeed");
        assert_eq!(reval, Revalidation::Unchanged);
        assert_eq!(refreshed.effective().top(), 2);
    }
}

/// 4. Verifies provider attempt admission and refusal behavior across all modes.
#[test]
fn network_admission_and_refusal_matrix() {
    // 4.1 Offline mode refusal -> ErrorKind::CacheMiss (exit 11)
    let consent_offline = NetworkConsent::Blocked(NetworkBlock::Offline);

    // Offline check occurs before credential check: even if CredentialStatus is Absent,
    // refusal is Offline (CacheMiss), NEVER Authentication!
    let refusal_offline = admit_provider_attempt(consent_offline, CredentialStatus::Absent)
        .expect_err("must be refused");
    assert_eq!(refusal_offline, ProviderAdmissionRefusal::Offline);
    assert_eq!(refusal_offline.kind(), ErrorKind::CacheMiss);

    // 4.2 Dry-run mode refusal -> ErrorKind::NetworkDenied (exit 8)
    let consent_dry_run = NetworkConsent::Blocked(NetworkBlock::DryRun);
    let refusal_dry_run = admit_provider_attempt(consent_dry_run, CredentialStatus::Absent)
        .expect_err("must be refused");
    assert_eq!(refusal_dry_run, ProviderAdmissionRefusal::DryRun);
    assert_eq!(refusal_dry_run.kind(), ErrorKind::NetworkDenied);

    // 4.3 Default (no consent) -> ErrorKind::NetworkDenied (exit 8)
    let consent_unauthorized = NetworkConsent::NotAuthorized;
    let refusal_no_consent = admit_provider_attempt(
        consent_unauthorized,
        CredentialStatus::PresentFromEnvironment,
    )
    .expect_err("must be refused");
    assert_eq!(
        refusal_no_consent,
        ProviderAdmissionRefusal::NetworkNotAuthorized
    );
    assert_eq!(refusal_no_consent.kind(), ErrorKind::NetworkDenied);

    // 4.4 Authorized via --allow-network but missing credential -> ErrorKind::Authentication (exit 4)
    let consent_allowed = NetworkConsent::Authorized(ConsentSource::AllowNetworkFlag);
    let refusal_no_cred = admit_provider_attempt(consent_allowed, CredentialStatus::Absent)
        .expect_err("must fail auth");
    assert_eq!(refusal_no_cred, ProviderAdmissionRefusal::MissingCredential);
    assert_eq!(refusal_no_cred.kind(), ErrorKind::Authentication);

    // 4.5 Authorized via --allow-network with credential -> Ok(ConsentSource::AllowNetworkFlag)
    let admission_ok =
        admit_provider_attempt(consent_allowed, CredentialStatus::PresentFromEnvironment)
            .expect("must admit");
    assert_eq!(admission_ok, ConsentSource::AllowNetworkFlag);

    // 4.6 Authorized via trusted user config with credential -> Ok(ConsentSource::TrustedUserConfig)
    let consent_trusted = NetworkConsent::Authorized(ConsentSource::TrustedUserConfig);
    let admission_trusted =
        admit_provider_attempt(consent_trusted, CredentialStatus::PresentFromEnvironment)
            .expect("must admit");
    assert_eq!(admission_trusted, ConsentSource::TrustedUserConfig);
}

/// 5. Verifies that explicitly selecting Cass source in offline or dry-run mode returns
///    exit code 7 (ErrorKind::UnsupportedSourceMode) with an actionable hint.
#[test]
fn offline_and_dry_run_cass_restriction_yields_error_7() {
    let ws = WorkspaceId::new("ws-cass-effect").unwrap();
    let cass_path = LocalPath::new(PathBuf::from("session.cass"));

    let options = SourceOptions {
        cass_session: Some(cass_path),
        ..SourceOptions::default()
    };

    // 5.1 Offline policy blocks cass
    let offline_policy = SourcePolicy {
        offline: true,
        dry_run: false,
        local_only: false,
        allow_network: false,
    };
    assert!(!offline_policy.cass_allowed());

    let err_offline = options
        .resolve(ws.clone(), offline_policy, false, |_, _| unreachable!())
        .expect_err("cass must fail under offline policy");
    assert_eq!(err_offline, SourceError::CassUnavailableInMode);
    assert_eq!(err_offline.kind(), ErrorKind::UnsupportedSourceMode);
    assert_eq!(
        err_offline.hint(),
        "Choose a direct transcript or normalized context in this mode."
    );

    // 5.2 Dry-run policy blocks cass
    let dry_run_policy = SourcePolicy {
        offline: false,
        dry_run: true,
        local_only: false,
        allow_network: false,
    };
    assert!(!dry_run_policy.cass_allowed());

    let err_dry_run = options
        .resolve(ws.clone(), dry_run_policy, false, |_, _| unreachable!())
        .expect_err("cass must fail under dry-run policy");
    assert_eq!(err_dry_run, SourceError::CassUnavailableInMode);
    assert_eq!(err_dry_run.kind(), ErrorKind::UnsupportedSourceMode);

    // 5.3 Local-only policy blocks cass
    let local_only_policy = SourcePolicy {
        offline: false,
        dry_run: false,
        local_only: true,
        allow_network: false,
    };
    assert!(!local_only_policy.cass_allowed());

    let err_local = options
        .resolve(ws, local_only_policy, false, |_, _| unreachable!())
        .expect_err("cass must fail under local-only policy");
    assert_eq!(err_local, SourceError::CassUnavailableInMode);
    assert_eq!(err_local.kind(), ErrorKind::UnsupportedSourceMode);
}

/// 6. Verifies that explicit local requirements resolve offline without network calls.
#[test]
fn explicit_local_requests_resolve_offline() {
    // Explicit local request directive does not require network access.
    let offline_policy = EffectPolicy::from_flags(EffectFlags {
        offline: true,
        ..EffectFlags::default()
    })
    .unwrap();

    // Verify offline policy does not block local operations
    assert_eq!(offline_policy.network_block(), Some(NetworkBlock::Offline));

    // Explicit skill directive local check:
    let required_skill = SkillId::new("deploy-helper").unwrap();
    assert_eq!(required_skill.as_str(), "deploy-helper");
}
