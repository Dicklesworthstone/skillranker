//! Platform-specific admission, shared by cache and ledger descriptor walks.

pub(super) use crate::filesystem_policy::local_filesystem;
pub(crate) use crate::platform_path::storage_path;

pub(super) type DirectoryIdentity = (nix::libc::dev_t, nix::libc::ino_t);
