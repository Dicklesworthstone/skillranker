#!/usr/bin/env bash
# SkillRanker installer. Inspect before running.
# curl -fsSL "https://raw.githubusercontent.com/Dicklesworthstone/skillranker/main/install.sh?$(date +%s)" | bash
set -euo pipefail
shopt -s lastpipe 2>/dev/null || true
umask 022
# Child installers/build tools have no reason to inherit a ranking credential.
unset TYPESAFE_API_KEY

REPO=Dicklesworthstone/skillranker
BASE="https://github.com/$REPO"
DEST="$HOME/.local/bin"
VERSION='' SOURCE='' OFFLINE='' CHECKSUM='' COSIGN_KEY=''
QUIET=0 NO_GUM=0 FORCE=0 EASY=0 VERIFY=0 CONFIGURE=1 KEEP_TEMP=0
FROM_SOURCE=0 LOCKED=0 TEMP='' LOCK='' HAS_GUM=0 COLOR=0 PINNED=0
PROXY_ARGS=()

usage() {
    cat <<'USAGE'
SkillRanker installer — skill advice powered by TypeSafe.ai Jev
Usage: bash install.sh [options]
  --version VERSION    Pin a release (v0.1.0 or 0.1.0); never switch versions
  --dest DIR           Binary directory (default: ~/.local/bin)
  --from-source        Build the selected tag, or main if no version is given
  --source DIR         Build this trusted checkout instead (implies --from-source)
  --offline TARBALL    Install a local archive; never download or build
  --sha256 HEX         Trusted archive digest (otherwise adjacent .sha256 required)
  --cosign-key FILE    Require a Sigstore bundle verified with this trusted DSR key
  --easy-mode          Back up and update the current shell's PATH configuration
  --no-configure       Skip agent skills and shell completions
  --verify             Also run local capabilities and offline demo checks
  --force              Reinstall, allowing downgrade; existing files are backed up
  --keep-temp          Retain staging and lock receipts for inspection
  --quiet              Print only errors
  --no-gum             Use plain/ANSI output even when gum is installed
  -h, --help           Show this help

Requires Bash, Python 3, install, and sha256sum or shasum. Online acquisition
also needs curl; source builds need Git, Rust and the pinned toolchain.
RCH is used when installed; remote failure never falls back to a local build.
Linux release targets use musl; macOS targets use the native architecture.
No releases available? The online installer builds main from source.

TypeSafe.ai account and YOUR API key are required for fresh ranking:
https://console.typesafe.ai . Installation does not load credentials, grant
network consent, install hooks, or send session content. Run sr doctor --json.
USAGE
}
die() { printf 'sr installer: %s\n' "$*" >&2; exit 1; }
value() { [[ $# -ge 2 && -n "$2" && "$2" != --* ]] || die "$1 needs a value"; }
while [[ $# -gt 0 ]]; do
    case "$1" in
        --version) value "$@"; VERSION="${2#v}"; PINNED=1; shift 2 ;;
        --dest) value "$@"; DEST="$2"; shift 2 ;;
        --source) value "$@"; SOURCE="$2"; FROM_SOURCE=1; shift 2 ;;
        --offline) value "$@"; OFFLINE="$2"; shift 2 ;;
        --sha256) value "$@"; CHECKSUM="$2"; shift 2 ;;
        --cosign-key) value "$@"; COSIGN_KEY="$2"; shift 2 ;;
        --from-source) FROM_SOURCE=1; shift ;;
        --easy-mode) EASY=1; shift ;;
        --no-configure) CONFIGURE=0; shift ;;
        --verify) VERIFY=1; shift ;;
        --force) FORCE=1; shift ;;
        --keep-temp) KEEP_TEMP=1; shift ;;
        --quiet) QUIET=1; shift ;;
        --no-gum) NO_GUM=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) die "Unknown option: $1 (see --help)" ;;
    esac
done
[[ -z "$VERSION" || "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9.-]+)?$ ]] || die 'Invalid version'
[[ -z "$OFFLINE" || "$FROM_SOURCE" == 0 ]] || die '--offline conflicts with source builds'
[[ -z "$SOURCE" || -z "$VERSION" ]] || die '--source and --version cannot be combined'
[[ "$FROM_SOURCE" == 0 || ( -z "$CHECKSUM" && -z "$COSIGN_KEY" ) ]] || die 'Archive verification flags cannot authenticate a source build'
for tool in python3 install mktemp; do command -v "$tool" >/dev/null || die "Required tool missing: $tool"; done
if [[ -t 2 && -z "${NO_COLOR+x}" && -z "${CI:-}" && "${TERM:-dumb}" != dumb ]]; then
    COLOR=1
    if [[ "$NO_GUM" == 0 ]] && command -v gum >/dev/null; then HAS_GUM=1; fi
fi
log() {
    local color="$1" label="$2"; shift 2
    [[ "$QUIET" == 0 ]] || return 0
    if [[ "$HAS_GUM" == 1 ]]; then gum style --foreground "$color" "$label $*" >&2
    elif [[ "$COLOR" == 1 ]]; then printf '\033[36m%s\033[0m %s\n' "$label" "$*" >&2
    else printf '%s %s\n' "$label" "$*" >&2; fi
}
info() { log 39 '→' "$@"; }
ok() { log 42 '✓' "$@"; }
warn() { log 214 '!' "$@"; }
err() { printf '✗ %s\n' "$*" >&2; }
draw_box() {
    [[ "$QUIET" == 0 ]] || return 0
    local line width=0 border='' i
    for line in "$@"; do [[ ${#line} -le $width ]] || width=${#line}; done
    for ((i=0; i<width+2; i++)); do border+='═'; done
    printf '╔%s╗\n' "$border" >&2
    for line in "$@"; do printf '║ %-*s ║\n' "$width" "$line" >&2; done
    printf '╚%s╝\n' "$border" >&2
}
run_with_spinner() {
    local title="$1"; shift
    info "$title"
    if [[ "$HAS_GUM" == 1 && "$QUIET" == 0 ]]; then gum spin --title "$title" -- "$@"
    else "$@"; fi
}
run_logged() {
    local title="$1"; shift
    run_with_spinner "$title" python3 -c \
        'import subprocess,sys; log=open(sys.argv[1],"wb"); sys.exit(subprocess.call(sys.argv[2:],stdout=log,stderr=log))' \
        "$TEMP/build.log" "$@"
}
sha256() {
    if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d ' ' -f1
    elif command -v shasum >/dev/null; then shasum -a 256 "$1" | cut -d ' ' -f1
    else die 'Install sha256sum or shasum; checksum verification is mandatory'; fi
}
# All binary probes have a deadline and receive no provider credential.
probe() {
    python3 - "$@" <<'PY'
import os, subprocess, sys
env = {k: v for k, v in os.environ.items() if not k.startswith(('TYPESAFE_', 'SR_'))}
try:
    p = subprocess.run(sys.argv[1:], env=env, stdin=subprocess.DEVNULL,
                       stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=10)
    if p.returncode:
        sys.stderr.buffer.write(p.stderr[:8192])
        sys.exit(1)
    sys.stdout.buffer.write(p.stdout)
except (OSError, subprocess.TimeoutExpired):
    sys.exit(1)
PY
}
download() {
    curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL \
        --connect-timeout 5 --max-time 120 --max-filesize 209715200 \
        ${PROXY_ARGS[@]+"${PROXY_ARGS[@]}"} "$1" -o "$2" 2>"$TEMP/download.stderr"
}
cleanup() {
    local status=$?
    # Keep a receipt instead of deleting a lock another process could acquire.
    if [[ "$LOCKED" == 1 ]]; then mv "$LOCK" "$TEMP/finished-lock" || true; fi
    if [[ -n "$TEMP" ]]; then
        if [[ "$KEEP_TEMP" == 1 || "$status" != 0 ]]; then info "Staging retained: $TEMP"
        else python3 - "$TEMP" <<'PY'
import shutil, sys
shutil.rmtree(sys.argv[1])
PY
        fi
    fi
    exit "$status"
}

draw_box 'SkillRanker installer' 'Session-specific skill advice powered by TypeSafe.ai Jev'
OS=$(uname -s); ARCH=$(uname -m); TARGET=''
case "$ARCH" in amd64) ARCH=x86_64 ;; arm64) ARCH=aarch64 ;; esac
case "$OS/$ARCH" in
    Linux/x86_64|Linux/aarch64) TARGET="$ARCH-unknown-linux-musl" ;;
    Darwin/x86_64|Darwin/aarch64) TARGET="$ARCH-apple-darwin" ;;
    *) [[ -z "$OFFLINE" ]] || die 'Offline archives support Linux/macOS x86_64/aarch64'; FROM_SOURCE=1 ;;
esac
if [[ "$OS" == Linux && -r /proc/version ]] && grep -qi microsoft /proc/version; then
    warn 'WSL detected; install into the Linux home and use the Linux agent environment.'
fi
[[ -z "${HTTPS_PROXY:-}" ]] || PROXY_ARGS=(--proxy "$HTTPS_PROXY")
if [[ ${#PROXY_ARGS[@]} == 0 && -n "${HTTP_PROXY:-}" ]]; then PROXY_ARGS=(--proxy "$HTTP_PROXY"); fi
[[ "$DEST" != *$'\n'* && "$DEST" != *$'\r'* ]] || die 'Destination contains a line break'
mkdir -p "$DEST"
DEST=$(cd "$DEST" && pwd -P)
[[ -w "$DEST" && ! -L "$DEST/sr" ]] || die 'Destination must be writable and sr must not be a symlink'
AVAIL=$(df -Pk "$DEST" | awk 'NR==2 {print $4}')
[[ "$AVAIL" =~ ^[0-9]+$ && "$AVAIL" -ge 262144 ]] || die 'Need at least 256 MiB free at destination'
TEMP=$(mktemp -d "$DEST/.sr-install.XXXXXXXX")
chmod 700 "$TEMP"
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
LOCK="$DEST/.sr-install.lock"
if ! mkdir "$LOCK" 2>/dev/null; then
    # Do not steal a possibly-live lock: even a dead PID needs operator review.
    if [[ -f "$LOCK/pid" ]]; then
        OLD_PID=$(cat "$LOCK/pid")
        if [[ "$OLD_PID" =~ ^[1-9][0-9]*$ ]] && ! kill -0 "$OLD_PID" 2>/dev/null; then
            die "Stale install lock at $LOCK. Inspect it, then move it aside and retry."
        fi
    fi
    die "Another installer owns $LOCK; retry after it exits"
fi
LOCKED=1
printf '%s\n' "$$" > "$LOCK/pid"

verify_archive() {
    local archive="$1" asset="$2" expected="$CHECKSUM"
    if [[ -z "$expected" ]]; then
        [[ -f "$archive.sha256" ]] || die 'Archive checksum sidecar missing; supply --sha256 from a trusted source'
        expected=$(python3 - "$archive.sha256" "$asset" <<'PY'
import re, sys
from pathlib import Path
p = Path(sys.argv[1])
if p.stat().st_size > 65536: sys.exit(1)
matches = []
for line in p.read_text().splitlines():
    parts = line.split()
    if len(parts) == 1 or (len(parts) == 2 and parts[1].lstrip('*') == sys.argv[2]):
        if parts and re.fullmatch('[0-9a-fA-F]{64}', parts[0]): matches.append(parts[0].lower())
if len(matches) != 1: sys.exit(1)
print(matches[0])
PY
        ) || die 'Checksum sidecar must uniquely identify this archive'
    fi
    expected=$(printf '%s' "$expected" | tr 'A-F' 'a-f')
    [[ "$expected" =~ ^[0-9a-f]{64}$ && "$(sha256 "$archive")" == "$expected" ]] || die 'SHA256 verification failed; existing installation preserved'
    ok 'Archive SHA256 verified'
    if [[ -n "$COSIGN_KEY" ]]; then
        command -v cosign >/dev/null || die '--cosign-key requires cosign'
        [[ -f "$COSIGN_KEY" && -f "$archive.sigstore.json" ]] || die 'Trusted public key or required Sigstore bundle is missing'
        # DSR uses an operator-trusted key; no Rekor service is required for
        # this offline key-signature check. Do not claim transparency proof.
        probe cosign verify-blob --offline --insecure-ignore-tlog=true --key "$COSIGN_KEY" --bundle "$archive.sigstore.json" "$archive" >"$TEMP/signature.log" || die 'Sigstore verification failed'
        ok 'DSR release signature verified with supplied trusted key'
    else
        warn 'Publisher signature not verified: no pinned release key is configured; use --cosign-key with a trusted DSR key.'
    fi
}
extract_archive() {
    python3 - "$1" "$TEMP" <<'PY'
import pathlib, sys, tarfile
archive, dest = sys.argv[1], pathlib.Path(sys.argv[2])
allowed = {'sr', 'LICENSE', 'README.md', 'skills/skillranker/SKILL.md',
           'completions/sr.bash', 'completions/_sr', 'completions/sr.fish'}
try:
    seen, size = set(), 0
    with tarfile.open(archive, 'r:*') as t:
        for count, member in enumerate(t, 1):
            name = member.name
            if count > 100 or name in seen or name.startswith('/') or '..' in name.split('/'):
                raise ValueError('unsafe or duplicate member')
            seen.add(name)
            if member.isdir() and name.rstrip('/') in {'skills', 'skills/skillranker', 'completions'}:
                continue
            if name not in allowed or not member.isfile() or member.size < 0:
                raise ValueError('unexpected member or non-regular file')
            size += member.size
            if size > 200*1024*1024 or (name != 'sr' and member.size > 262144):
                raise ValueError('archive size limit')
            out = dest / 'payload' / name
            out.parent.mkdir(parents=True, exist_ok=True)
            with t.extractfile(member) as src, out.open('xb') as dst:
                remaining = member.size
                while remaining:
                    chunk = src.read(min(65536, remaining))
                    if not chunk: raise ValueError('truncated member')
                    dst.write(chunk); remaining -= len(chunk)
        if 'sr' not in seen: raise ValueError('missing sr')
except (OSError, ValueError, tarfile.TarError) as e:
    sys.exit('sr installer: archive rejected: ' + str(e))
PY
    BIN="$TEMP/payload/sr"
    chmod 755 "$BIN"
}
build_source() {
    [[ -z "$COSIGN_KEY" && -z "$CHECKSUM" ]] || die 'Refusing source fallback under archive authentication flags'
    command -v cargo >/dev/null || die 'Source build requires Rust: install the repository-pinned toolchain first'
    if [[ -z "$SOURCE" ]]; then
        command -v git >/dev/null || die 'Source build requires git'
        SOURCE="$TEMP/source"
        run_logged 'Fetching source (see build.log on failure)' git clone --depth 1 --single-branch \
            --branch "${VERSION:+v}${VERSION:-main}" "$BASE.git" "$SOURCE" || die "Source fetch failed; log: $TEMP/build.log (use --keep-temp)"
    fi
    SOURCE=$(cd "$SOURCE" && pwd -P)
    [[ -f "$SOURCE/Cargo.lock" && -f "$SOURCE/rust-toolchain.toml" ]] || die 'Source must be a SkillRanker checkout with lockfile and pinned toolchain'
    # A unique target avoids accidentally installing an old local build after RCH.
    BUILD_TARGET="$TEMP/target"
    if command -v rch >/dev/null; then
        (cd "$SOURCE" && run_logged 'Building sr remotely; this can take several minutes' env RCH_REQUIRE_REMOTE=1 rch exec -- cargo build --release --locked --bin sr --target-dir "$BUILD_TARGET") || die "Remote build failed; log: $TEMP/build.log (use --keep-temp)"
    else
        (cd "$SOURCE" && run_logged 'Building sr locally; this can take several minutes' cargo build --release --locked --bin sr --target-dir "$BUILD_TARGET") || die "Source build failed; log: $TEMP/build.log (use --keep-temp)"
    fi
    BIN="$BUILD_TARGET/release/sr"
    [[ -f "$BIN" ]] || die 'Build returned without a local artifact. Check RCH artifact transfer; no stale binary installed.'
}

BIN=''
if [[ -n "$OFFLINE" ]]; then
    [[ -f "$OFFLINE" ]] || die 'Offline archive does not exist'
    python3 - "$OFFLINE" <<'PY'
import os, sys
if os.stat(sys.argv[1]).st_size > 200*1024*1024:
    sys.exit('sr installer: compressed archive exceeds 200 MiB')
PY
    # Snapshot input and sidecars before verification/extraction.
    cp "$OFFLINE" "$TEMP/archive.tar.gz"
    for suffix in sha256 sigstore.json; do
        if [[ -f "$OFFLINE.$suffix" ]]; then cp "$OFFLINE.$suffix" "$TEMP/archive.tar.gz.$suffix"; fi
    done
    verify_archive "$TEMP/archive.tar.gz" "$(basename "$OFFLINE")"
    extract_archive "$TEMP/archive.tar.gz"
elif [[ "$FROM_SOURCE" == 1 ]]; then
    build_source
else
    command -v curl >/dev/null || die 'Online installation requires curl'
    if [[ -z "$VERSION" ]]; then
        info 'Resolving the latest GitHub release'
        if download "https://api.github.com/repos/$REPO/releases/latest" "$TEMP/release.json"; then
            VERSION=$(python3 -c 'import json,sys; tag=json.load(open(sys.argv[1]))["tag_name"]; print(tag[1:] if tag.startswith("v") else tag)' "$TEMP/release.json")
        else
            REDIRECT=$(curl --proto '=https' --proto-redir '=https' -fsSL --connect-timeout 5 --max-time 15 ${PROXY_ARGS[@]+"${PROXY_ARGS[@]}"} -o /dev/null -w '%{url_effective}' "$BASE/releases/latest" 2>"$TEMP/redirect.stderr") || REDIRECT=''
            if [[ "$REDIRECT" == "$BASE/releases/tag/v"* ]]; then VERSION="${REDIRECT##*/v}"; fi
        fi
    fi
    if [[ -n "$VERSION" ]]; then
        [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9.-]+)?$ ]] || die 'Release service returned an invalid version'
        # All fallbacks remain bound to the SAME selected release.
        for asset in "skillranker-v$VERSION-$TARGET.tar.gz" "skillranker-$TARGET.tar.gz" "skillranker-$(printf '%s' "$OS" | tr '[:upper:]' '[:lower:]')-$ARCH.tar.gz"; do
            URL="$BASE/releases/download/v$VERSION/$asset"
            info "Fetching $asset"
            if download "$URL" "$TEMP/archive.tar.gz"; then
                if [[ -z "$CHECKSUM" ]]; then download "$URL.sha256" "$TEMP/archive.tar.gz.sha256" || die 'Downloaded archive has no checksum; refusing source fallback'; fi
                if [[ -n "$COSIGN_KEY" ]]; then download "$URL.sigstore.json" "$TEMP/archive.tar.gz.sigstore.json" || die 'Required signature download failed'; fi
                verify_archive "$TEMP/archive.tar.gz" "$asset"
                extract_archive "$TEMP/archive.tar.gz"
                break
            fi
        done
    fi
    if [[ -z "$BIN" ]]; then warn 'No release artifact available; building the selected source revision'; build_source; fi
fi

NEW_VERSION=$(probe "$BIN" --version) || die 'Candidate binary cannot run on this machine; existing installation preserved'
[[ "$NEW_VERSION" =~ ^sr\ ([0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9.-]+)?)$ ]] || die 'Candidate did not identify itself as sr'
[[ -z "$VERSION" || "$NEW_VERSION" == "sr $VERSION" ]] || die 'Binary version does not match selected release'
probe "$BIN" capabilities --json >"$TEMP/capabilities.json" || die 'Candidate capabilities check failed'
python3 - "$TEMP/capabilities.json" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
if not any(c.get('name') == 'rank' and c.get('status') == 'implemented' for c in d['commands']):
    sys.exit('sr installer: rank is not implemented in the candidate')
PY
if [[ "$VERIFY" == 1 ]]; then probe "$BIN" demo --case useful --json >"$TEMP/demo.json" || die 'Offline self-test failed'; fi
BACKUP='none'
if [[ -e "$DEST/sr" ]]; then
    [[ -f "$DEST/sr" ]] || die 'Existing sr is not a regular file'
    OLD_VERSION=$(probe "$DEST/sr" --version) || OLD_VERSION=''
    if [[ "$PINNED" == 0 && "$FORCE" == 0 ]]; then
        python3 - "$OLD_VERSION" "$NEW_VERSION" <<'PY'
import re, sys
def version(s):
    m = re.fullmatch(r'sr (\d+)\.(\d+)\.(\d+)(?:-([A-Za-z0-9.-]+))?', s)
    if not m: return None
    # Stable sorts after prerelease; different prereleases require explicit intent.
    return tuple(map(int, m.groups()[:3])), m[4]
old, new = map(version, sys.argv[1:])
if old and new and (old[0] > new[0] or (old[0] == new[0] and old[1] != new[1] and new[1] is not None)):
    sys.exit('sr installer: refusing an implicit downgrade; use --version or --force deliberately')
PY
    fi
    if [[ "$FORCE" == 0 && "$(sha256 "$DEST/sr")" == "$(sha256 "$BIN")" ]]; then
        ok 'Identical sr already installed; refreshing optional integrations'
    else
        BACKUP="$DEST/sr.backup.$(date -u +%Y%m%dT%H%M%SZ).$$"
        cp -p "$DEST/sr" "$BACKUP"
        install -m 0755 "$BIN" "$TEMP/sr.ready"
        python3 -c 'import os,sys; os.replace(sys.argv[1], sys.argv[2])' "$TEMP/sr.ready" "$DEST/sr"
    fi
else
    install -m 0755 "$BIN" "$TEMP/sr.ready"
    python3 -c 'import os,sys; os.replace(sys.argv[1], sys.argv[2])' "$TEMP/sr.ready" "$DEST/sr"
fi
ok "Installed $NEW_VERSION at $DEST/sr"

if [[ "$CONFIGURE" == 1 || "$EASY" == 1 ]]; then
    info 'Inspecting installed agent directories and shell integration'
    if ! python3 - "$TEMP" "$DEST" "$CONFIGURE" "$EASY" "$QUIET" <<'PY'
import json, os, pathlib, shlex, shutil, sys, time
stage, dest = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
configure, easy, quiet = (x == '1' for x in sys.argv[3:])
home = pathlib.Path.home()
def report(text):
    if not quiet: print(text, file=sys.stderr)
def write(path, text):
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        if path.is_symlink(): return 'skipped symlink (configure manually)'
        if path.exists():
            if path.read_text() == text: return 'already present'
            return 'preserved existing file (configure manually)'
        # Exclusive creation: never overwrite a raced-in user file.
        with path.open('x') as f: f.write(text)
        return 'created'
    except (OSError, UnicodeError):
        return 'failed (binary installed; configure manually)'
commands = [c['name'] for c in json.load(open(stage/'capabilities.json'))['commands']
            if c['status'] == 'implemented' and c['name'] not in ('help', 'version')]
words = ' '.join(commands)
skill = '''---
name: skillranker
description: Use sr for session-specific skill recommendations powered by TypeSafe.ai Jev.
---
# SkillRanker
Run `sr capabilities --json`, `sr doctor --json`, and `sr rank --help` first.
Fresh ranking requires the user's own TYPESAFE_API_KEY from https://console.typesafe.ai
AND explicit trusted network consent. Never print credentials. Installation grants no consent.
Use an exact source: `sr rank --context FILE --roster FILE --dry-run` previews locally;
`--allow-network --json` requests a live evaluation. Context is the normalized schema;
arbitrary text/JSON is not interchangeable. See the repository README and schemas.
Codex needs an explicit supported roster; native hook support must be checked in capabilities.
Do not run a skill automatically. Recommendations are advisory, may be unverified, and
must respect user exclusions. `--offline` makes no calls; `--no-persist` disables storage.
Use `sr demo --case useful` to inspect an explicitly synthetic offline example.
Docs: https://github.com/Dicklesworthstone/skillranker
'''
if configure:
    bundled = stage/'payload/skills/skillranker/SKILL.md'
    if bundled.is_file():
        try: skill = bundled.read_text()
        except (OSError, UnicodeError): report('Bundled skill unreadable; using inline guide')
    for agent, directory in [('Claude Code', '.claude'), ('Codex', '.codex')]:
        if (home/directory).is_dir():
            status = write(home/directory/'skills/skillranker/SKILL.md', skill)
            report(f'{agent}: usage skill {status}; hooks not configured')
        else: report(f'{agent}: not detected; skipped')
    for agent, directory in [('Gemini', '.gemini'), ('Cursor', '.cursor'), ('Continue', '.continue')]:
        report(f'{agent}: '+('detected; use normalized context manually' if (home/directory).is_dir() else 'not detected; skipped'))
    data = pathlib.Path(os.environ.get('XDG_DATA_HOME', str(home/'.local/share')))
    config = pathlib.Path(os.environ.get('XDG_CONFIG_HOME', str(home/'.config')))
    # Subcommand completions are derived from the installed build's capabilities.
    for path, text in [
        (data/'bash-completion/completions/sr', f"complete -W '{words}' sr\n"),
        (data/'zsh/site-functions/_sr', f"#compdef sr\n_arguments '1:command:({words})' '*:file:_files'\n"),
        (config/'fish/completions/sr.fish', '\n'.join(f"complete -c sr -n '__fish_use_subcommand' -a '{c}'" for c in commands)+'\n')]:
        report('Completion '+str(path)+': '+write(path, text))
if str(dest) not in os.environ.get('PATH', '').split(os.pathsep):
    shell = pathlib.Path(os.environ.get('SHELL', '/bin/bash')).name
    if shell == 'fish':
        rc = home/'.config/fish/config.fish'; line = 'fish_add_path -- '+shlex.quote(str(dest))
    else:
        rc = home/('.zshrc' if shell == 'zsh' else '.bashrc')
        line = 'export PATH='+shlex.quote(str(dest))+':"$PATH"'
    if easy:
        if rc.is_symlink(): raise ValueError('Refusing symlink shell rc')
        old = rc.read_text() if rc.exists() else ''
        if line not in old.splitlines():
            rc.parent.mkdir(parents=True, exist_ok=True)
            if rc.exists():
                backup = rc.with_name(rc.name+f'.sr-backup.{time.time_ns()}')
                shutil.copy2(rc, backup); report('Shell backup: '+str(backup))
            with rc.open('a') as f: f.write('\n# SkillRanker PATH\n'+line+'\n')
        report('PATH configured; open a new shell. External concurrent shell-rc edits are unsupported.')
    else: report('Add to your shell configuration: '+line)
PY
    then
        err 'Binary installed; optional configuration failed. Configure your shell manually; diagnostics are above.'
    fi
fi
draw_box 'SkillRanker installation complete' "Binary: $DEST/sr" "Previous binary backup: $BACKUP" \
    'Next: sr doctor --json; obtain your own key at console.typesafe.ai' \
    'Hooks remain unconfigured. Network requires separate consent.'
info "Uninstall: remove $DEST/sr; restore the reported backup to revert."
info 'Optional integrations: remove only the skillranker skill, sr completion files, and SkillRanker PATH line created here.'
