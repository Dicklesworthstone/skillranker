# Installing SkillRanker

Use `bash install.sh --help` for the complete option list. The installer writes
`sr` to `~/.local/bin` by default; `--dest DIR` selects another directory. It
requires Bash and Python 3, standard POSIX utilities, and either `sha256sum` or
`shasum`. Online acquisition requires curl. There is no automatic sudo or Rust
toolchain installation.

```bash
bash install.sh --source . --verify
bash install.sh --offline /path/to/skillranker.tar.gz --verify
bash install.sh --version v0.1.0 --dest "$HOME/.local/bin" --verify
```

The version example selects that release if it exists; it does not establish
that the tag or its assets have been published. At the initial installer
verification, the repository had no public releases. The source and offline
paths were exercised on Linux x86_64. The macOS and Linux aarch64 selection
rules are implemented, but those platforms have not been executed in this
installer verification. No signed multi-platform release is claimed.

## Acquisition and verification

An explicit `--version` wins. Otherwise the online path resolves the latest
GitHub release through the API, then the release redirect. If neither yields a
release, it builds `main`. Source fallback for an existing release stays on
that release's tag; it never silently changes a pinned version to latest.
`--source DIR` builds that explicit trusted checkout and cannot be combined
with `--version`. Cargo consumes the checkout's toolchain and lockfile.

Release archives use these names, in preference order, under the same tag:

1. `skillranker-vVERSION-TARGET.tar.gz`
2. `skillranker-TARGET.tar.gz`
3. `skillranker-OS-ARCH.tar.gz`

Linux selects `x86_64-unknown-linux-musl` or `aarch64-unknown-linux-musl`;
macOS selects the corresponding `apple-darwin` target. Source builds use the
native Rust target and are not represented as portable musl release artifacts.
Other platforms use source fallback. WSL uses the Linux path.

Every downloaded or offline archive needs its adjacent `.sha256` file, or an
explicit `--sha256 HEX` from a trusted source. The sidecar contains exactly
one matching `HASH  ARCHIVE_NAME` record, or a single bare hash. Verification
failure is fatal: it never falls through to source compilation. HTTPS redirects
cannot downgrade to HTTP. Curl honors `HTTPS_PROXY`, `HTTP_PROXY`, and
`NO_PROXY`; proxy credentials are not printed.

SHA256 detects corruption; a digest obtained beside an archive is not an
independent publisher signature. No SkillRanker release signing key is pinned
in this installer yet. For DSR-signed bundles, supply `--cosign-key FILE` with
an independently trusted public key. This requires cosign and the adjacent
`.sigstore.json` bundle. Missing or invalid signatures fail closed, including
offline installation, with no source fallback. Without a trusted key, the
installer explicitly reports that publisher signature verification was not
performed. There is no GitHub Actions trust or build fallback.

An archive contains one root-level regular file named `sr`. Optional members
are `LICENSE`, `README.md`, `skills/skillranker/SKILL.md`, and
`completions/sr.bash`, `completions/_sr`, `completions/sr.fish`. Links, devices,
absolute paths, traversal, duplicate members, and unexpected files are refused.
The archive is bounded to 100 members and 200 MiB unpacked; ancillary files
are bounded to 256 KiB each. Files are streamed into private staging, never
extracted over user directories.

## Source builds and replacement

When available, RCH builds with `RCH_REQUIRE_REMOTE=1`; failed remote builds
never silently compile locally. Without RCH, standalone installations use
Cargo directly. A unique target directory prevents a successful-looking build
from installing a stale local binary. Missing downloaded build artifacts fail
explicitly. `--keep-temp` retains the build log and staging for diagnosis.

The candidate must run, identify itself as `sr`, match a requested version,
and expose implemented ranking in `capabilities --json` before replacement.
`--verify` also runs an offline demo. These probes have time limits and do not
inherit TypeSafe credentials. A passing demo is not a live-provider test.

Installation uses a destination-scoped atomic directory lock. A live lock is
never stolen, even with `--force`; stale or malformed locks require inspection
before the operator moves them aside. The candidate is staged with mode 0755
on the destination filesystem and renamed into place. Existing binaries get a
uniquely named backup. Byte-identical installs skip replacement but still check
optional integrations. Implicit downgrades are refused; use an explicit version
or `--force` only when a downgrade is intentional.

## Agent and shell setup

For existing `~/.claude` and `~/.codex` directories, the installer adds
`skills/skillranker/SKILL.md`. It uses the skill from the verified binary archive
when supplied, otherwise an inline guide. It never replaces a customized skill.
Other detected agents are reported with manual normalized-context guidance.
There is no daemon, predecessor migration, or guessed native hook to configure.
Consult `sr capabilities --json` for the installed build's actual support.

Completions list only implemented subcommands reported by that binary. They go
to the XDG Bash, Zsh, and Fish completion directories. Existing completion files
are preserved. Zsh users must have its XDG `site-functions` directory in `fpath`
before `compinit`; Bash users need their completion loader. `--easy-mode` updates
PATH for the current shell and backs up an existing rc file. It does not load
the file into the current shell. Concurrent external rc-file edits are not
supported. `--no-configure` skips agent skills and completions.

Installation does not read `.env`, create or copy API keys, authorize network
traffic, or enable hooks. Sign up at [TypeSafe.ai](https://console.typesafe.ai)
for your own key and follow [runtime setup](../README.md#runtime-setup).

## Rollback and removal

The final summary names the installed binary and previous-binary backup. Restore
that backup to the reported binary path to roll back. To uninstall, remove the
installed `sr` executable. Remove only the SkillRanker skill, `sr` completion
files, and marked PATH line added by this installer if you also want to undo
integration. Preserve unrelated shell and agent settings. API keys, caches,
ledgers, and project data are not removed by the installer.

## Verification

```bash
bash -n install.sh
shellcheck install.sh
SR_INSTALL_TEST_BINARY=/absolute/path/to/sr python3 tests/install_contract.py -v
```

The tests use fixture executables for installer failure boundaries and a
separately supplied real release binary for actual installation and demo checks.
They retain scratch directories and use `--keep-temp`; no test cleans up the
user's files. Public release download/signature success and cross-platform
execution require their own real artifacts before those claims can be made.
