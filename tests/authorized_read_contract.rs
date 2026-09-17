use skillranker::authorized_read::*;
use skillranker::identity::ContentHash;
use skillranker::limits::{LimitUnit, ResourceLimit, SKILL_FILE_BYTES};
use skillranker::privacy::{TrustedAbsoluteRoot, WorkspaceRelativeRoot};
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const CANARY: &str = "outside-content-canary-0123456789";

static SEQUENCE: AtomicU32 = AtomicU32::new(0);

/// Temporary trees are retained for inspection, like the other suites here.
fn temp_tree(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let unique = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "sr-authread-{name}-{}-{nanos}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("create temporary tree");
    path
}

fn write(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent");
    }
    fs::write(path, content).expect("write fixture");
}

fn root_of(path: &Path) -> AuthorizedRoot {
    AuthorizedRoot::open_absolute(path).expect("open authorized root")
}

fn limit_of(max: usize) -> ResourceLimit {
    ResourceLimit::try_new("test_file", LimitUnit::Bytes, max).expect("positive limit")
}

fn read(roots: &AuthorizedRoots, relative: &str) -> Result<BoundedRead, ReadError> {
    roots.read_bounded(0, Path::new(relative), SKILL_FILE_BYTES)
}

#[test]
fn authorized_regular_file_reads_bind_hash_identity_and_bytes() {
    let tree = temp_tree("happy");
    write(&tree.join("skills/a/SKILL.md"), "---\nname: a\n---\nbody\n");
    let roots = AuthorizedRoots::single(root_of(&tree));

    let skill = read(&roots, "skills/a/SKILL.md").expect("legitimate read");
    assert_eq!(skill.bytes(), b"---\nname: a\n---\nbody\n");
    assert_eq!(skill.len(), 21);
    assert!(!skill.is_empty());
    assert_eq!(
        skill.content_hash(),
        &ContentHash::from_bytes(skill.bytes())
    );
    assert_eq!(skill.path().as_path(), tree.join("skills/a/SKILL.md"));
    // Private Debug: no bytes, no path.
    let debug = format!("{skill:?}");
    assert!(!debug.contains("body"), "{debug}");
    assert!(!debug.contains("SKILL.md"), "{debug}");

    // A second name for the same file shares its identity and hash.
    fs::hard_link(tree.join("skills/a/SKILL.md"), tree.join("skills/alias.md")).expect("hard link");
    let alias = read(&roots, "skills/alias.md").expect("hard alias read");
    assert_eq!(alias.identity(), skill.identity());
    assert_eq!(alias.content_hash(), skill.content_hash());
    assert_ne!(alias.path().as_path(), skill.path().as_path());
}

#[test]
fn reads_stop_at_the_byte_cap_and_hash_only_returned_bytes() {
    let tree = temp_tree("bounds");
    write(&tree.join("exact.md"), "12345678");
    write(&tree.join("over.md"), "123456789");
    write(&tree.join("empty.md"), "");
    let roots = AuthorizedRoots::single(root_of(&tree));
    let limit = limit_of(8);

    let exact = roots
        .read_bounded(0, Path::new("exact.md"), limit)
        .expect("file at the cap is readable");
    assert_eq!(exact.bytes(), b"12345678");
    assert_eq!(exact.content_hash(), &ContentHash::from_bytes(b"12345678"));
    let empty = roots
        .read_bounded(0, Path::new("empty.md"), limit)
        .expect("empty file is a valid read");
    assert!(empty.is_empty());
    assert_eq!(
        roots.read_bounded(0, Path::new("over.md"), limit),
        Err(ReadError::TooLarge { limit: 8 })
    );
}

#[test]
fn content_hash_changes_when_size_and_mtime_do_not() {
    let tree = temp_tree("rewrite");
    let path = tree.join("skill.md");
    write(&path, "aaaa");
    let roots = AuthorizedRoots::single(root_of(&tree));
    let first = read(&roots, "skill.md").expect("first read");
    let stamp = fs::metadata(&path)
        .expect("metadata")
        .modified()
        .expect("mtime");

    write(&path, "bbbb");
    fs::File::options()
        .write(true)
        .open(&path)
        .expect("reopen for mtime restore")
        .set_modified(stamp)
        .expect("restore mtime");
    let metadata = fs::metadata(&path).expect("metadata");
    assert_eq!(metadata.len(), 4, "same size");
    assert_eq!(metadata.modified().expect("mtime"), stamp, "same mtime");

    let second = read(&roots, "skill.md").expect("second read");
    assert_eq!(second.identity(), first.identity(), "same inode");
    assert_ne!(
        second.content_hash(),
        first.content_hash(),
        "metadata must not decide content identity"
    );
}

#[test]
fn only_regular_files_are_read() {
    let tree = temp_tree("types");
    write(&tree.join("regular.md"), "ok");
    fs::create_dir_all(tree.join("directory")).expect("create directory");
    let fifo = tree.join("pipe");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("run mkfifo");
    assert!(status.success(), "mkfifo failed");
    let roots = AuthorizedRoots::single(root_of(&tree));

    assert!(read(&roots, "regular.md").is_ok(), "successful counterpart");
    assert_eq!(
        read(&roots, "directory"),
        Err(ReadError::NotRegularFile(FileKind::Directory))
    );
    // A FIFO must be refused without blocking on a writer that never arrives.
    assert_eq!(
        read(&roots, "pipe"),
        Err(ReadError::NotRegularFile(FileKind::Fifo))
    );

    // Character devices are refused even when their directory is authorized.
    let devices = AuthorizedRoots::single(root_of(Path::new("/dev")));
    assert_eq!(
        devices.read_bounded(0, Path::new("null"), SKILL_FILE_BYTES),
        Err(ReadError::NotRegularFile(FileKind::CharacterDevice))
    );
}

#[test]
fn symlinks_resolve_only_inside_authorized_roots() {
    let tree = temp_tree("links");
    let outside = temp_tree("links-outside");
    write(&tree.join("skills/real.md"), "inside");
    write(&outside.join("secret.md"), CANARY);
    symlink("real.md", tree.join("skills/relative.md")).expect("relative link");
    symlink(tree.join("skills/real.md"), tree.join("skills/absolute.md")).expect("absolute link");
    symlink(outside.join("secret.md"), tree.join("skills/escape.md")).expect("escaping link");
    symlink("../../", tree.join("skills/up")).expect("parent link");
    symlink("cycle_b.md", tree.join("skills/cycle_a.md")).expect("cycle a");
    symlink("cycle_a.md", tree.join("skills/cycle_b.md")).expect("cycle b");
    let roots = AuthorizedRoots::single(root_of(&tree));

    // Legitimate links inside the root resolve.
    assert_eq!(
        read(&roots, "skills/relative.md")
            .expect("relative")
            .bytes(),
        b"inside"
    );
    assert_eq!(
        read(&roots, "skills/absolute.md")
            .expect("absolute")
            .bytes(),
        b"inside"
    );

    // Escapes and cycles are refused.
    let escaped = read(&roots, "skills/escape.md").expect_err("escape must fail");
    assert_eq!(escaped, ReadError::EscapesAuthorizedRoots);
    assert_eq!(
        read(&roots, "skills/up/secret.md"),
        Err(ReadError::EscapesAuthorizedRoots)
    );
    assert_eq!(
        read(&roots, "skills/cycle_a.md"),
        Err(ReadError::SymlinkLoop)
    );

    // A second authorized root makes a cross-root link legitimate.
    let both = AuthorizedRoots::new(vec![root_of(&tree), root_of(&outside)]);
    assert_eq!(
        both.read_bounded(0, Path::new("skills/escape.md"), SKILL_FILE_BYTES)
            .expect("authorized cross-root link")
            .bytes(),
        CANARY.as_bytes()
    );
}

#[test]
fn replacing_a_component_after_the_root_opens_cannot_redirect_reads() {
    let tree = temp_tree("swap");
    let outside = temp_tree("swap-outside");
    write(&tree.join("skills/SKILL.md"), "inside");
    write(&outside.join("SKILL.md"), CANARY);
    let roots = AuthorizedRoots::single(root_of(&tree));
    assert_eq!(
        read(&roots, "skills/SKILL.md")
            .expect("before swap")
            .bytes(),
        b"inside"
    );

    // Swap the intermediate directory for a symlink out of the root.
    fs::rename(tree.join("skills"), tree.join("skills-moved")).expect("move directory");
    symlink(&outside, tree.join("skills")).expect("swap in escaping link");
    assert_eq!(
        read(&roots, "skills/SKILL.md"),
        Err(ReadError::EscapesAuthorizedRoots),
        "a swapped component must never redirect the read"
    );

    // The held root descriptor still names the original directory even after
    // its name is replaced by a link to somewhere else.
    let renamed = temp_tree("swap-root");
    write(&renamed.join("file.md"), "original-root");
    let held = AuthorizedRoots::single(root_of(&renamed));
    fs::rename(&renamed, renamed.with_extension("moved")).expect("rename root");
    symlink(&outside, &renamed).expect("replace root path with a link");
    assert_eq!(
        held.read_bounded(0, Path::new("file.md"), SKILL_FILE_BYTES)
            .expect("descriptor-bound read")
            .bytes(),
        b"original-root"
    );
}

#[test]
fn concurrent_component_swaps_never_return_content_from_outside() {
    let tree = temp_tree("race");
    let outside = temp_tree("race-outside");
    write(&tree.join("real/SKILL.md"), "inside");
    write(&outside.join("SKILL.md"), CANARY);
    let roots = AuthorizedRoots::single(root_of(&tree));
    let link_path = tree.join("skills");
    symlink(tree.join("real"), &link_path).expect("initial link");

    let flipper_path = link_path.clone();
    let outside_path = outside.clone();
    let inside_path = tree.join("real");
    let flipper = std::thread::spawn(move || {
        for index in 0..300 {
            let _ = fs::remove_file(&flipper_path);
            let target = if index % 2 == 0 {
                &outside_path
            } else {
                &inside_path
            };
            let _ = symlink(target, &flipper_path);
            std::thread::yield_now();
        }
    });

    let mut inside = 0u32;
    let mut refused = 0u32;
    for _ in 0..300 {
        match read(&roots, "skills/SKILL.md") {
            Ok(found) => {
                assert_eq!(found.bytes(), b"inside", "read escaped its authorized root");
                inside += 1;
            }
            Err(error) => {
                // Every refusal is acceptable; leaking outside content is not.
                // A component swapped mid-walk can surface as NotADirectory
                // (O_DIRECTORY|O_NOFOLLOW reports ENOTDIR and the link is gone
                // before the type is confirmed), SymlinkLoop, NotFound or Io.
                assert!(
                    matches!(
                        error,
                        ReadError::EscapesAuthorizedRoots
                            | ReadError::NotADirectory
                            | ReadError::NotRegularFile(_)
                            | ReadError::SymlinkLoop
                            | ReadError::NotFound
                            | ReadError::Io
                    ),
                    "unexpected error {error:?}"
                );
                refused += 1;
            }
        }
    }
    flipper.join().expect("flipper thread");
    assert_eq!(inside + refused, 300);
}

#[test]
fn requested_paths_cannot_climb_out_and_missing_files_are_reported() {
    let tree = temp_tree("paths");
    write(&tree.join("skills/SKILL.md"), "inside");
    let roots = AuthorizedRoots::single(root_of(&tree));

    assert!(
        read(&roots, "skills/SKILL.md").is_ok(),
        "successful counterpart"
    );
    for request in ["../escape.md", "/etc/passwd", "skills/../../escape.md"] {
        assert_eq!(
            read(&roots, request),
            Err(ReadError::InvalidRelativePath),
            "{request}"
        );
    }
    assert_eq!(read(&roots, "skills/missing.md"), Err(ReadError::NotFound));
    assert_eq!(read(&roots, ""), Err(ReadError::InvalidRelativePath));
    assert_eq!(
        read(&roots, "skills/SKILL.md/child"),
        Err(ReadError::NotADirectory)
    );
}

#[test]
fn absolute_reads_require_an_authorized_root() {
    let tree = temp_tree("absolute");
    let outside = temp_tree("absolute-outside");
    write(&tree.join("skills/SKILL.md"), "inside");
    write(&outside.join("secret.md"), CANARY);
    let roots = AuthorizedRoots::single(root_of(&tree));

    assert_eq!(
        roots
            .read_absolute(&tree.join("skills/SKILL.md"), SKILL_FILE_BYTES)
            .expect("authorized absolute read")
            .bytes(),
        b"inside"
    );
    assert_eq!(
        roots.read_absolute(&outside.join("secret.md"), SKILL_FILE_BYTES),
        Err(ReadError::EscapesAuthorizedRoots)
    );
    assert_eq!(
        roots.read_absolute(Path::new("relative.md"), SKILL_FILE_BYTES),
        Err(ReadError::InvalidRelativePath)
    );
}

#[test]
fn workspace_relative_roots_open_without_leaving_the_workspace() {
    let tree = temp_tree("workspace");
    let outside = temp_tree("workspace-outside");
    write(&tree.join("nested/skills/SKILL.md"), "inside");
    write(&outside.join("SKILL.md"), CANARY);
    symlink(&outside, tree.join("linked")).expect("escaping directory link");
    let workspace = root_of(&tree);

    let nested = workspace
        .open_workspace_relative(&WorkspaceRelativeRoot::parse("nested/skills").expect("root"))
        .expect("nested root opens");
    assert_eq!(nested.absolute_path(), tree.join("nested/skills"));
    let roots = AuthorizedRoots::single(nested);
    assert_eq!(
        read(&roots, "SKILL.md").expect("nested read").bytes(),
        b"inside"
    );

    // "." resolves to the workspace root itself.
    let same = workspace
        .open_workspace_relative(&WorkspaceRelativeRoot::parse(".").expect("root"))
        .expect("workspace root");
    assert_eq!(same.identity(), workspace.identity());

    // A root that would leave the workspace through a link is refused.
    assert_eq!(
        workspace
            .open_workspace_relative(&WorkspaceRelativeRoot::parse("linked").expect("root"))
            .expect_err("escaping root"),
        ReadError::EscapesAuthorizedRoots
    );

    // Trusted absolute roots use the same opener.
    let trusted = AuthorizedRoot::open_trusted(
        &TrustedAbsoluteRoot::parse(outside.to_str().expect("utf-8 temp path")).expect("root"),
    )
    .expect("trusted root");
    assert_eq!(trusted.absolute_path(), outside.as_path());
}

#[test]
fn native_path_bytes_survive_and_diagnostics_stay_private() {
    let tree = temp_tree("bytes");
    let name = OsStr::from_bytes(b"caf\xff.md");
    let path = tree.join(name);
    fs::write(&path, "non-utf8 name").expect("write fixture");
    let roots = AuthorizedRoots::single(root_of(&tree));

    let found = roots
        .read_bounded(0, Path::new(name), SKILL_FILE_BYTES)
        .expect("non-UTF-8 name is readable");
    assert_eq!(found.bytes(), b"non-utf8 name");
    assert_eq!(found.path().as_path(), path);
    assert!(found.path().as_path().to_str().is_none(), "bytes preserved");
    assert_eq!(format!("{:?}", found.path()), "LocalPath(<private>)");

    // No error text names a path, a link target or file content.
    let outside = temp_tree("bytes-outside");
    write(&outside.join("secret.md"), CANARY);
    symlink(outside.join("secret.md"), tree.join("escape.md")).expect("escaping link");
    for error in [
        read_err(&roots, "escape.md"),
        read_err(&roots, "missing.md"),
        read_err(&roots, "../escape.md"),
    ] {
        let rendered = format!("{error} {error:?}");
        assert!(!rendered.contains(CANARY), "{rendered}");
        assert!(!rendered.contains("secret.md"), "{rendered}");
        assert!(
            !rendered.contains(tree.to_str().unwrap_or("/")),
            "{rendered}"
        );
    }
}

fn read_err(roots: &AuthorizedRoots, relative: &str) -> ReadError {
    read(roots, relative).expect_err("expected failure")
}
