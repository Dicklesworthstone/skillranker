//! Platform path spelling shared by coordination and persistent stores.
//!
//! This is not filesystem admission. In particular it does not canonicalize
//! arbitrary symlinks or remove parent components; callers retain their own
//! descriptor, no-follow, ownership, and filesystem checks.

use std::path::PathBuf;

/// Expand only macOS's verified root-owned /tmp and /var system aliases.
pub(crate) fn storage_path(path: PathBuf) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::MetadataExt;
        use std::path::Path;
        expand_system_alias(path, |alias, target| {
            std::fs::symlink_metadata(alias).is_ok_and(|metadata| {
                metadata.file_type().is_symlink()
                    && metadata.uid() == 0
                    && std::fs::read_link(alias)
                        .is_ok_and(|p| p == Path::new(target) || p == Path::new(&target[1..]))
            })
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        path
    }
}

// The alias spelling policy is testable on every host, but filesystem evidence
// comes only from the macOS implementation above. No runtime override exists.
#[cfg(any(target_os = "macos", test))]
fn expand_system_alias(path: PathBuf, trusted: impl Fn(&str, &str) -> bool) -> PathBuf {
    for (alias, target) in [("/tmp", "/private/tmp"), ("/var", "/private/var")] {
        if let Ok(rest) = path.strip_prefix(alias)
            && trusted(alias, target)
        {
            return std::path::Path::new(target).join(rest);
        }
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verified_aliases_expand_with_their_suffix() {
        for (input, expected) in [
            ("/tmp", "/private/tmp"),
            ("/tmp/sr/cache.sqlite3", "/private/tmp/sr/cache.sqlite3"),
            ("/var", "/private/var"),
            (
                "/var/folders/sr/ledger.sqlite3",
                "/private/var/folders/sr/ledger.sqlite3",
            ),
        ] {
            assert_eq!(
                expand_system_alias(PathBuf::from(input), |_, _| true),
                PathBuf::from(expected)
            );
        }
    }

    #[test]
    fn unverified_aliases_are_not_rewritten() {
        for input in ["/tmp", "/tmp/cache.sqlite3", "/var/folders/sr"] {
            let path = PathBuf::from(input);
            assert_eq!(expand_system_alias(path.clone(), |_, _| false), path);
        }
    }

    #[test]
    fn unrelated_and_lookalike_paths_do_not_probe_aliases() {
        for input in [
            "",
            "tmp/sr",
            "/tmp-other/sr",
            "/various/sr",
            "/private/tmp/sr",
            "/home/sr",
        ] {
            let path = PathBuf::from(input);
            assert_eq!(
                expand_system_alias(path.clone(), |_, _| panic!("unexpected alias probe")),
                path
            );
        }
    }

    #[test]
    fn suffix_is_not_canonicalized_or_admitted() {
        for (input, expected) in [
            (
                "/tmp/user-link/cache.sqlite3",
                "/private/tmp/user-link/cache.sqlite3",
            ),
            ("/var/../untrusted", "/private/var/../untrusted"),
        ] {
            assert_eq!(
                expand_system_alias(PathBuf::from(input), |_, _| true),
                PathBuf::from(expected)
            );
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn non_macos_path_spelling_is_unchanged() {
        for input in ["/tmp/sr", "/var/sr", "/private/tmp/sr", "relative/sr"] {
            let path = PathBuf::from(input);
            assert_eq!(storage_path(path.clone()), path);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_system_aliases_expand_but_user_symlinks_do_not() {
        use std::os::unix::fs::{DirBuilderExt, symlink};
        assert_eq!(
            storage_path(PathBuf::from("/tmp")),
            PathBuf::from("/private/tmp")
        );
        assert_eq!(
            storage_path(PathBuf::from("/var")),
            PathBuf::from("/private/var")
        );
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "sr-platform-path-{}-{nonce}",
            std::process::id()
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        let alias = root.join("user-alias");
        symlink("/private/tmp", &alias).unwrap();
        let expanded = storage_path(alias.clone());
        assert!(
            std::fs::symlink_metadata(expanded)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        std::fs::remove_file(alias).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
}
