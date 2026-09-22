//! Managed harness hook installation and rollback.
//!
//! Satisfies contract requirements for bead `sr-roadmap-l1i.7.3`:
//! - Preview and apply only the managed Claude settings entry.
//! - Uses trusted absolute binary path, quoted arguments, UserPromptSubmit, and explicit timeout.
//! - Preserves unrelated settings, other hooks, and file permissions.
//! - Owner-only backups in private state directory before publication.
//! - Per-target locking to serialize cooperating installers.
//! - Base digest checking to abort on detected external changes.
//! - Idempotent installation.
//! - Refuses malformed settings without overwriting them.
//! - Reports enterprise / managed restrictions without bypassing.
//! - Displays conflicts for modified entries rather than broad removal.

use crate::config::HookMode;
use crate::identity::ContentHash;
use crate::limits::CONFIG_FILE_BYTES;
use crate::output::JsonSeed;
use nix::fcntl::{Flock, FlockArg};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::fmt;
use std::fs::{self, File, OpenOptions};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The harness timeout written into the managed hook entry.
///
/// It must STRICTLY exceed the internal deadline, not merely match it. The internal clock starts at
/// process entry, so everything before it — process spawn, dynamic linking, TLS state, configuration
/// reads — is outside the deadline but inside the harness timeout. At three seconds against a
/// 3,000 ms deadline the harness could kill `sr` during the final 200 ms it reserves for writing its
/// answer and cleaning up, which is the one window where being killed loses work rather than merely
/// timing out. Four seconds is also what README documents (sr-83cc).
pub const DEFAULT_HOOK_TIMEOUT_SECS: u32 = 4;

/// Seconds reserved between the installed outer timeout and the internal
/// ranking deadline for process startup before the entry clock begins.
pub const HOOK_STARTUP_RESERVE_SECS: u32 = 1;

/// The smallest harness timeout that can bound a given internal deadline.
///
/// Ceiling of the deadline in whole seconds, plus one: a 3,000 ms deadline needs 4 s, and a 3,200 ms
/// deadline needs 5 s. The extra second is the startup headroom described above, and expressing
/// it as a function rather than a constant means a later deadline change cannot silently invalidate
/// the installed entry — which is exactly what AGENTS.md forbids.
pub const fn minimum_hook_timeout_secs(deadline_ms: u64) -> u32 {
    let whole = deadline_ms.div_ceil(1_000);
    (whole + 1) as u32
}

/// Why a requested harness timeout cannot be installed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimeoutTooTight {
    pub requested_secs: u32,
    pub deadline_ms: u64,
    pub minimum_secs: u32,
}

impl std::fmt::Display for TimeoutTooTight {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "--timeout-secs {} cannot bound sr's own {} ms deadline: the internal clock \
             starts after process startup, so install with at least --timeout-secs {} or the harness \
             can kill the run inside the window reserved for writing its answer",
            self.requested_secs, self.deadline_ms, self.minimum_secs
        )
    }
}

/// Refuses a harness timeout that does not strictly exceed the internal deadline.
pub fn check_hook_timeout(requested_secs: u32, deadline_ms: u64) -> Result<(), TimeoutTooTight> {
    let minimum_secs = minimum_hook_timeout_secs(deadline_ms);
    if requested_secs >= minimum_secs {
        return Ok(());
    }
    Err(TimeoutTooTight {
        requested_secs,
        deadline_ms,
        minimum_secs,
    })
}

/// The internal ranking budget implied by the installed managed entry:
/// its outer timeout minus the startup reserve, in milliseconds. `None`
/// when no exactly matching managed entry is installed or its settings
/// cannot be read; callers fall back to the conservative default budget.
pub fn installed_hook_budget_ms(
    harness: HookHarness,
    binary_path_override: Option<PathBuf>,
) -> Option<u64> {
    let settings_path = resolve_settings_file(harness, None).ok()?;
    let (settings, _, _) = read_and_parse_settings(&settings_path).ok()?;
    let entries = settings.get("hooks")?.get("UserPromptSubmit")?.as_array()?;
    let binary_path = resolve_binary_path(binary_path_override).ok()?;
    let command = make_command_string(&binary_path);
    for item in entries {
        let Some(hooks) = item.get("hooks").and_then(Value::as_array) else {
            continue;
        };
        let Some(hook) = hooks.iter().find(|h| {
            h.get("command")
                .and_then(Value::as_str)
                .is_some_and(|cmd| cmd == command)
        }) else {
            continue;
        };
        let timeout_secs = hook.get("timeout").and_then(Value::as_u64)?;
        return Some(timeout_secs.saturating_sub(u64::from(HOOK_STARTUP_RESERVE_SECS)) * 1_000);
    }
    None
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookHarness {
    Claude,
}

impl HookHarness {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "claude" => Some(Self::Claude),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
        }
    }
}

impl fmt::Display for HookHarness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallOptions {
    pub apply: bool,
    pub harness: HookHarness,
    pub settings_file: Option<PathBuf>,
    pub timeout_secs: u32,
    pub binary_path: Option<PathBuf>,
    pub effective_mode: HookMode,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InstallOutcome {
    Preview {
        diff: String,
        message: String,
    },
    Applied {
        backup_path: PathBuf,
        message: String,
    },
    AlreadyInstalled {
        message: String,
    },
    Conflict {
        message: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UninstallOutcome {
    Preview {
        diff: String,
        message: String,
    },
    Applied {
        backup_path: PathBuf,
        message: String,
    },
    NotInstalled {
        message: String,
    },
    Conflict {
        message: String,
    },
}

#[derive(Debug)]
pub enum InstallerError {
    InvalidUsage(String),
    MalformedSettings(String),
    EnterpriseRestricted(String),
    ExternalModificationDetected(String),
    LockBusy(String),
    Io(std::io::Error),
}

impl fmt::Display for InstallerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUsage(msg) => write!(f, "invalid usage: {msg}"),
            Self::MalformedSettings(msg) => write!(f, "malformed settings: {msg}"),
            Self::EnterpriseRestricted(msg) => write!(f, "enterprise restricted: {msg}"),
            Self::ExternalModificationDetected(msg) => {
                write!(f, "external modification detected: {msg}")
            }
            Self::LockBusy(msg) => write!(f, "lock busy: {msg}"),
            Self::Io(err) => write!(f, "I/O error: {err}"),
        }
    }
}

impl std::error::Error for InstallerError {}

impl From<std::io::Error> for InstallerError {
    fn from(err: std::io::Error) -> Self {
        if err.kind() == std::io::ErrorKind::PermissionDenied {
            Self::EnterpriseRestricted("permission denied on settings target".into())
        } else {
            Self::Io(err)
        }
    }
}

/// Resolves the trusted absolute path to the settings file for the harness.
pub fn resolve_settings_file(
    harness: HookHarness,
    override_path: Option<PathBuf>,
) -> Result<PathBuf, InstallerError> {
    if let Some(path) = override_path {
        return Ok(path);
    }
    match harness {
        HookHarness::Claude => {
            let home = std::env::var_os("HOME")
                .filter(|p| !p.is_empty())
                .map(PathBuf::from)
                .ok_or_else(|| {
                    InstallerError::InvalidUsage("HOME environment variable is not set".into())
                })?;
            Ok(home.join(".claude/settings.json"))
        }
    }
}

/// Resolves the trusted absolute path to the `sr` binary.
pub fn resolve_binary_path(override_path: Option<PathBuf>) -> Result<PathBuf, InstallerError> {
    if let Some(path) = override_path {
        if !path.is_absolute() {
            return Err(InstallerError::InvalidUsage(
                "provided binary path must be absolute".into(),
            ));
        }
        return Ok(path);
    }
    let exe = std::env::current_exe().map_err(InstallerError::Io)?;
    if !exe.is_absolute() {
        return Err(InstallerError::InvalidUsage(
            "current executable path is not absolute".into(),
        ));
    }
    Ok(exe)
}

/// Resolves the private state directory used for owner-only backups.
pub fn resolve_backup_dir() -> Result<PathBuf, InstallerError> {
    let base = if let Some(state) = std::env::var_os("XDG_STATE_HOME").filter(|p| !p.is_empty()) {
        PathBuf::from(state).join("sr/backups")
    } else if let Some(home) = std::env::var_os("HOME").filter(|p| !p.is_empty()) {
        PathBuf::from(home).join(".local/state/sr/backups")
    } else {
        PathBuf::from("/tmp/sr-backups")
    };

    fs::create_dir_all(&base)?;
    #[cfg(unix)]
    {
        let _ = fs::set_permissions(&base, fs::Permissions::from_mode(0o700));
    }
    Ok(base)
}

/// Writes an owner-only backup of original settings bytes before publication.
pub fn write_backup(
    settings_file: &Path,
    original_bytes: &[u8],
) -> Result<PathBuf, InstallerError> {
    let backup_dir = resolve_backup_dir()?;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let filename = format!(
        "{}.sr-backup.{}_{}_{}",
        settings_file
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("settings.json"),
        now.as_secs(),
        now.subsec_nanos(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let backup_path = backup_dir.join(filename);

    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&backup_path)?;

    #[cfg(unix)]
    {
        let _ = fs::set_permissions(&backup_path, fs::Permissions::from_mode(0o600));
    }

    use std::io::Write;
    file.write_all(original_bytes)?;
    file.sync_all()?;
    Ok(backup_path)
}

/// Formats the managed hook command invocation string.
pub fn make_command_string(binary_path: &Path) -> String {
    let path_str = binary_path.to_string_lossy();
    if path_str.contains(' ') {
        format!("\"{path_str}\" hook claude")
    } else {
        format!("{path_str} hook claude")
    }
}

/// Formats the managed Claude hook entry JSON value.
pub fn make_managed_entry(command_str: &str, timeout_secs: u32) -> Value {
    json!({
        "matcher": "",
        "hooks": [
            {
                "type": "command",
                "command": command_str,
                "timeout": timeout_secs
            }
        ]
    })
}

/// Checks whether a command string is an invocation of `sr hook claude`.
fn is_sr_hook_command(command: &str) -> bool {
    let trimmed = command.trim();
    trimmed.ends_with("hook claude") || trimmed.contains("hook claude")
}

#[derive(Debug, Eq, PartialEq)]
enum MatcherEntryClassification {
    ExactMatch(usize),
    Conflict(usize, String),
    None,
}

/// Classifies any existing SkillRanker entry in `UserPromptSubmit`.
fn classify_existing_entry(
    user_prompt_submit: &[Value],
    expected_command: &str,
    expected_timeout: u32,
) -> MatcherEntryClassification {
    for (idx, item) in user_prompt_submit.iter().enumerate() {
        let Some(matcher_obj) = item.as_object() else {
            continue;
        };
        let Some(hooks_arr) = matcher_obj.get("hooks").and_then(Value::as_array) else {
            continue;
        };

        // Check if any hook inside looks like our hook claude invocation
        let has_sr_hook = hooks_arr.iter().any(|h| {
            h.get("command")
                .and_then(Value::as_str)
                .is_some_and(is_sr_hook_command)
        });

        if !has_sr_hook {
            continue;
        }

        // Found a candidate. Does it match exactly?
        if hooks_arr.len() == 1 {
            let h = &hooks_arr[0];
            let cmd = h.get("command").and_then(Value::as_str).unwrap_or("");
            let timeout = h.get("timeout").and_then(Value::as_u64).unwrap_or(0) as u32;
            let matcher = matcher_obj
                .get("matcher")
                .and_then(Value::as_str)
                .unwrap_or("");

            if matcher.is_empty() && cmd == expected_command && timeout == expected_timeout {
                return MatcherEntryClassification::ExactMatch(idx);
            }
            return MatcherEntryClassification::Conflict(
                idx,
                format!(
                    "found command '{cmd}' (timeout {timeout}s), expected '{expected_command}' (timeout {expected_timeout}s)"
                ),
            );
        } else {
            return MatcherEntryClassification::Conflict(
                idx,
                "matcher entry contains multiple hooks alongside sr hook".to_string(),
            );
        }
    }
    MatcherEntryClassification::None
}

/// Generates a unified diff preview for adding the managed hook entry.
pub fn generate_install_diff(settings_path: &Path, entry: &Value) -> String {
    let pretty = serde_json::to_string_pretty(entry).unwrap_or_default();
    let mut diff = String::new();
    diff.push_str(&format!("--- {} (before)\n", settings_path.display()));
    diff.push_str(&format!("+++ {} (after)\n", settings_path.display()));
    diff.push_str("@@ hooks.UserPromptSubmit @@\n");
    for line in pretty.lines() {
        diff.push_str(&format!("+ {line}\n"));
    }
    diff
}

/// Generates a unified diff preview for removing the managed hook entry.
pub fn generate_uninstall_diff(settings_path: &Path, entry: &Value) -> String {
    let pretty = serde_json::to_string_pretty(entry).unwrap_or_default();
    let mut diff = String::new();
    diff.push_str(&format!("--- {} (before)\n", settings_path.display()));
    diff.push_str(&format!("+++ {} (after)\n", settings_path.display()));
    diff.push_str("@@ hooks.UserPromptSubmit @@\n");
    for line in pretty.lines() {
        diff.push_str(&format!("- {line}\n"));
    }
    diff
}

/// Parsed settings, plus the exact bytes and content hash they were read from when
/// the file existed. An absent file yields an empty object and no byte evidence.
type ParsedSettings = (Value, Option<Vec<u8>>, Option<ContentHash>);

/// Reads and validates `settings.json`, enforcing bounded size and duplicate-key rejection.
fn read_and_parse_settings(path: &Path) -> Result<ParsedSettings, InstallerError> {
    if !path.exists() {
        return Ok((Value::Object(Map::new()), None, None));
    }

    let bytes = fs::read(path)?;
    if bytes.len() > CONFIG_FILE_BYTES.max() {
        return Err(InstallerError::MalformedSettings(format!(
            "settings file exceeds maximum size bound of {} bytes",
            CONFIG_FILE_BYTES.max()
        )));
    }

    if bytes.iter().all(|b| b.is_ascii_whitespace()) {
        let hash = ContentHash::from_bytes(&bytes);
        return Ok((Value::Object(Map::new()), Some(bytes), Some(hash)));
    }

    let mut decoder = serde_json::Deserializer::from_slice(&bytes);
    use serde::de::DeserializeSeed;
    let val = JsonSeed(0)
        .deserialize(&mut decoder)
        .map_err(|e| InstallerError::MalformedSettings(format!("invalid JSON: {e}")))?;

    if !val.is_object() {
        return Err(InstallerError::MalformedSettings(
            "root settings value must be a JSON object".into(),
        ));
    }

    let hash = ContentHash::from_bytes(&bytes);
    Ok((val, Some(bytes), Some(hash)))
}

/// RAII lock guard that serializes cooperating installers using `flock`.
struct SettingsLockGuard {
    _flock: Flock<File>,
}

impl SettingsLockGuard {
    fn acquire(settings_path: &Path) -> Result<Self, InstallerError> {
        let lock_path = settings_path.with_extension("json.sr-lock");
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;

        let flock = Flock::lock(file, FlockArg::LockExclusiveNonblock).map_err(|_| {
            InstallerError::LockBusy(format!(
                "concurrent installer holds lock on {}",
                lock_path.display()
            ))
        })?;

        Ok(Self { _flock: flock })
    }
}

/// Publishes modified settings JSON to `settings_file` atomically via temporary file and rename.
fn publish_settings_atomically(
    settings_file: &Path,
    new_value: &Value,
    expected_base_hash: Option<ContentHash>,
) -> Result<(), InstallerError> {
    // Check if target was modified externally during our work
    if settings_file.exists() {
        let current_bytes = fs::read(settings_file)?;
        let current_hash = ContentHash::from_bytes(&current_bytes);
        if expected_base_hash != Some(current_hash) {
            return Err(InstallerError::ExternalModificationDetected(
                "settings file was modified externally during installation; aborting to prevent lost updates".into(),
            ));
        }
    } else if expected_base_hash.is_some() {
        return Err(InstallerError::ExternalModificationDetected(
            "settings file was removed externally during installation; aborting".into(),
        ));
    }

    let parent = settings_file.parent().ok_or_else(|| {
        InstallerError::InvalidUsage("settings file has no parent directory".into())
    })?;
    fs::create_dir_all(parent)?;

    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let tmp_path = parent.join(format!(
        "{}.sr-tmp.{}_{}_{}",
        settings_file
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("settings.json"),
        std::process::id(),
        now.subsec_nanos(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));

    let pretty_bytes = serde_json::to_vec_pretty(new_value)
        .map_err(|e| InstallerError::MalformedSettings(e.to_string()))?;

    {
        let mut tmp_file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp_path)?;

        #[cfg(unix)]
        {
            // Preserve original permissions if original existed, else 0600
            let mode = if settings_file.exists() {
                fs::metadata(settings_file)
                    .map(|m| m.permissions().mode())
                    .unwrap_or(0o600)
            } else {
                0o600
            };
            let _ = fs::set_permissions(&tmp_path, fs::Permissions::from_mode(mode));
        }

        use std::io::Write;
        tmp_file.write_all(&pretty_bytes)?;
        tmp_file.write_all(b"\n")?;
        tmp_file.sync_all()?;
    }

    fs::rename(&tmp_path, settings_file).map_err(|e| {
        let _ = fs::remove_file(&tmp_path);
        InstallerError::Io(e)
    })?;

    Ok(())
}

/// Executes hook installation preview or apply for Claude Code.
pub fn install_hook(options: &InstallOptions) -> Result<InstallOutcome, InstallerError> {
    if options.harness != HookHarness::Claude {
        return Err(InstallerError::InvalidUsage(format!(
            "Hook installation for {} is not supported in this release",
            options.harness
        )));
    }

    let settings_path = resolve_settings_file(options.harness, options.settings_file.clone())?;
    let binary_path = resolve_binary_path(options.binary_path.clone())?;
    let command_str = make_command_string(&binary_path);
    let managed_entry = make_managed_entry(&command_str, options.timeout_secs);

    let (mut settings, original_bytes, base_hash) = read_and_parse_settings(&settings_path)?;

    // Ensure `hooks` object exists
    let hooks_obj = settings
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));

    if !hooks_obj.is_object() {
        return Err(InstallerError::MalformedSettings(
            "existing 'hooks' property in settings must be a JSON object".into(),
        ));
    }

    // Ensure `UserPromptSubmit` array exists
    let user_prompt_submit = hooks_obj
        .as_object_mut()
        .unwrap()
        .entry("UserPromptSubmit")
        .or_insert_with(|| Value::Array(Vec::new()));

    if !user_prompt_submit.is_array() {
        return Err(InstallerError::MalformedSettings(
            "existing 'hooks.UserPromptSubmit' property in settings must be a JSON array".into(),
        ));
    }

    let arr = user_prompt_submit.as_array().unwrap();
    let classification = classify_existing_entry(arr, &command_str, options.timeout_secs);

    let mode_str = match options.effective_mode {
        HookMode::Shadow => "shadow (default, evaluations recorded without injecting context)",
        HookMode::Advisory => "advisory (suggestions enabled)",
    };

    match classification {
        MatcherEntryClassification::ExactMatch(_) => Ok(InstallOutcome::AlreadyInstalled {
            message: format!(
                "Managed Claude hook is already installed in {} and up to date (effective mode: {}).",
                settings_path.display(),
                mode_str
            ),
        }),
        MatcherEntryClassification::Conflict(idx, desc) => Ok(InstallOutcome::Conflict {
            message: format!(
                "Conflicting hook entry found at index {idx} in {}: {desc}. To avoid overwriting user customizations, remove or reconcile the entry manually.",
                settings_path.display()
            ),
        }),
        MatcherEntryClassification::None => {
            let diff = generate_install_diff(&settings_path, &managed_entry);
            if !options.apply {
                return Ok(InstallOutcome::Preview {
                    diff,
                    message: format!(
                        "Previewing hook installation for {}. Run with --apply to merge the managed entry (effective mode: {}).",
                        settings_path.display(),
                        mode_str
                    ),
                });
            }

            // Apply: acquire lock, write backup, update JSON, publish atomically
            let _lock = SettingsLockGuard::acquire(&settings_path)?;

            let backup_path = if let Some(bytes) = &original_bytes {
                write_backup(&settings_path, bytes)?
            } else {
                write_backup(&settings_path, b"{}")?
            };

            // Add new entry
            user_prompt_submit
                .as_array_mut()
                .unwrap()
                .push(managed_entry);

            publish_settings_atomically(&settings_path, &settings, base_hash)?;

            Ok(InstallOutcome::Applied {
                backup_path,
                message: format!(
                    "Successfully installed Claude hook into {} (effective mode: {}).",
                    settings_path.display(),
                    mode_str
                ),
            })
        }
    }
}

/// Executes hook uninstallation preview or apply for Claude Code.
pub fn uninstall_hook(
    harness: HookHarness,
    settings_file: Option<PathBuf>,
    binary_path: Option<PathBuf>,
    timeout_secs: u32,
    apply: bool,
) -> Result<UninstallOutcome, InstallerError> {
    if harness != HookHarness::Claude {
        return Err(InstallerError::InvalidUsage(format!(
            "Hook uninstallation for {harness} is not supported in this release"
        )));
    }

    let settings_path = resolve_settings_file(harness, settings_file)?;
    if !settings_path.exists() {
        return Ok(UninstallOutcome::NotInstalled {
            message: format!(
                "Settings file {} does not exist; no hook installed.",
                settings_path.display()
            ),
        });
    }

    let binary_path = resolve_binary_path(binary_path)?;
    let command_str = make_command_string(&binary_path);
    let (mut settings, original_bytes, base_hash) = read_and_parse_settings(&settings_path)?;

    let Some(hooks_obj) = settings.as_object_mut().unwrap().get_mut("hooks") else {
        return Ok(UninstallOutcome::NotInstalled {
            message: format!(
                "No managed Claude hook entry found in {} (no hooks configured).",
                settings_path.display()
            ),
        });
    };

    let Some(user_prompt_submit) = hooks_obj.get_mut("UserPromptSubmit") else {
        return Ok(UninstallOutcome::NotInstalled {
            message: format!(
                "No managed Claude hook entry found in {} (no UserPromptSubmit hooks configured).",
                settings_path.display()
            ),
        });
    };

    let Some(arr) = user_prompt_submit.as_array_mut() else {
        return Err(InstallerError::MalformedSettings(
            "hooks.UserPromptSubmit is not a JSON array".into(),
        ));
    };

    let classification = classify_existing_entry(arr, &command_str, timeout_secs);

    match classification {
        MatcherEntryClassification::None => Ok(UninstallOutcome::NotInstalled {
            message: format!(
                "No managed Claude hook entry found in {}.",
                settings_path.display()
            ),
        }),
        MatcherEntryClassification::Conflict(idx, desc) => Ok(UninstallOutcome::Conflict {
            message: format!(
                "Cannot uninstall entry at index {idx} in {}: {desc}. Refusing broad removal of a modified hook entry.",
                settings_path.display()
            ),
        }),
        MatcherEntryClassification::ExactMatch(idx) => {
            let removed_entry = arr[idx].clone();
            let diff = generate_uninstall_diff(&settings_path, &removed_entry);

            if !apply {
                return Ok(UninstallOutcome::Preview {
                    diff,
                    message: format!(
                        "Previewing hook uninstallation for {}. Run with --apply to remove only the managed entry.",
                        settings_path.display()
                    ),
                });
            }

            // Apply: acquire lock, write backup, remove entry, publish atomically
            let _lock = SettingsLockGuard::acquire(&settings_path)?;

            let backup_path = if let Some(bytes) = &original_bytes {
                write_backup(&settings_path, bytes)?
            } else {
                write_backup(&settings_path, b"{}")?
            };

            arr.remove(idx);

            // Clean up empty UserPromptSubmit or hooks if no hooks remain
            if arr.is_empty() {
                hooks_obj
                    .as_object_mut()
                    .unwrap()
                    .remove("UserPromptSubmit");
            }
            if hooks_obj.as_object().is_some_and(|h| h.is_empty()) {
                settings.as_object_mut().unwrap().remove("hooks");
            }

            publish_settings_atomically(&settings_path, &settings, base_hash)?;

            Ok(UninstallOutcome::Applied {
                backup_path,
                message: format!(
                    "Successfully removed managed Claude hook from {}.",
                    settings_path.display()
                ),
            })
        }
    }
}
