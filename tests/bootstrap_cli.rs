use std::process::Command;

#[test]
fn version_reports_package_version() {
    let version = Command::new(env!("CARGO_BIN_EXE_sr"))
        .arg("--version")
        .env_clear()
        .output()
        .unwrap();
    assert!(version.status.success());
    assert_eq!(
        version.stdout,
        concat!("sr ", env!("CARGO_PKG_VERSION"), "\n").as_bytes()
    );
    assert!(version.stderr.is_empty());
}

#[test]
fn unsupported_commands_fail_without_echoing_private_arguments() {
    for args in [
        vec![],
        vec!["rank", "private-canary"],
        vec!["--help", "private-canary"],
    ] {
        let result = Command::new(env!("CARGO_BIN_EXE_sr"))
            .env_clear()
            .args(args)
            .output()
            .unwrap();
        assert_eq!(result.status.code(), Some(2));
        let report: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(report["schema_version"], 1);
        assert_eq!(report["decision"], "unavailable");
        assert_eq!(report["error"]["code"], 2);
        assert_eq!(report["error"]["kind"], "invalid-usage");
        assert_eq!(report["error"]["retryable"], false);
        assert!(!String::from_utf8_lossy(&result.stdout).contains("private-canary"));
        assert!(result.stderr.is_empty());
    }
}

#[cfg(unix)]
#[test]
fn non_utf8_arguments_are_usage_errors_without_panicking() {
    use std::os::unix::ffi::OsStringExt;
    let result = Command::new(env!("CARGO_BIN_EXE_sr"))
        .env_clear()
        .arg(std::ffi::OsString::from_vec(vec![0xff]))
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(2));
    let report: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(report["decision"], "unavailable");
    assert_eq!(report["error"]["code"], 2);
    assert_eq!(report["error"]["kind"], "invalid-usage");
    assert!(result.stderr.is_empty());
}
