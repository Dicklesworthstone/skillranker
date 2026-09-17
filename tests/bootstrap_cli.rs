use std::process::Command;

#[test]
fn help_and_version_describe_only_implemented_commands() {
    let help = Command::new(env!("CARGO_BIN_EXE_sr"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(help.status.success());
    let text = String::from_utf8(help.stdout).unwrap();
    assert!(text.contains("Only help and version are available"));
    assert!(text.contains("your own API key"));
    assert!(help.stderr.is_empty());

    let version = Command::new(env!("CARGO_BIN_EXE_sr"))
        .arg("--version")
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
            .args(args)
            .output()
            .unwrap();
        assert_eq!(result.status.code(), Some(2));
        assert!(result.stdout.is_empty());
        assert_eq!(result.stderr, b"sr: unsupported arguments; use --help\n");
    }
}

#[cfg(unix)]
#[test]
fn non_utf8_arguments_are_usage_errors_without_panicking() {
    use std::os::unix::ffi::OsStringExt;
    let result = Command::new(env!("CARGO_BIN_EXE_sr"))
        .arg(std::ffi::OsString::from_vec(vec![0xff]))
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(2));
    assert!(result.stdout.is_empty());
}
