#!/usr/bin/env python3
"""Map product e2e case IDs to the real Rust tests that establish them.

A product suite runs whole Rust integration targets. Its catalog names, for
each contract-matrix case, the tests whose passing establishes that case.
`check` verifies the catalog statically against the sources and the matrix.
`evaluate` reads an actual cargo test log and reports each case passed,
failed or missing. A case never passes by name alone: every mapped test must
appear as `... ok`, and the whole run must be complete. Every unfiltered
target must execute tests, and its aggregate counts must reconcile with
all distinct result lines, including tests not mapped to an individual case.

Cargo's stderr and the test binaries' stdout interleave unpredictably, so
results are attributed by test name. `check` guarantees that every mapped name
is a test in exactly one of the suite's targets.

A test may re-run its own binary with `--exact NAME`, and with `--nocapture`
that child's output joins the log. A product run never filters, so only
unfiltered summaries count as target summaries. A repeated result line for one
name merges, and the worst status wins.
"""
import json
import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
MATRIX = ROOT / "tests/contract_matrix.toml"
# libtest appends a reason to ignored results: `... ignored, <reason>`.
RESULT = re.compile(r"^test (\S+) \.\.\. (ok|FAILED|ignored)(?:, .*)?$")
# The worst status wins when one name reports more than once.
RANK = {"ok": 0, "ignored": 1, "FAILED": 2}
SUMMARY = re.compile(
    r"^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; "
    r"(\d+) measured; (\d+) filtered out"
)
# A #[test] attribute, optional further attributes, then the function name.
TEST_FN = re.compile(r"#\[test\]\s*(?:#\[[^\]]*\]\s*)*fn\s+([a-z0-9_]+)\s*\(")
IGNORED_FN = re.compile(r"#\[ignore[^\]]*\]\s*(?:#\[[^\]]*\]\s*)*fn\s+([a-z0-9_]+)\s*\(")
ALL = "all"


# Structured product evidence is separate from the runner-mechanics schema.
MAX_ARTIFACT = 2 * 1024 * 1024
MAX_LOG = 64 * 1024 * 1024
CANARY = b'DO_NOT_LOG_PRODUCT_CANARY_7c391826'


def bounded_bytes(path, limit):
    import os
    import stat
    descriptor = os.open(path, os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW)
    with os.fdopen(descriptor, 'rb') as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_size > limit:
            raise ValueError('artifact-file-limit')
        data = stream.read(limit + 1)
        if len(data) > limit:
            raise ValueError('artifact-file-limit')
        return data


def artifact_json(data):
    if len(data) > MAX_ARTIFACT:
        raise ValueError('artifact-limit')
    def unique(pairs):
        value = {}
        for key, item in pairs:
            if key in value:
                raise ValueError('duplicate-definition')
            value[key] = item
        return value
    def invalid_constant(_):
        raise ValueError('nonfinite')
    value = json.loads(data, object_pairs_hook=unique, parse_constant=invalid_constant)
    pending = [(value, 0)]
    visited = 0
    while pending:
        item, depth = pending.pop()
        visited += 1
        if depth > 64 or visited > 100_000:
            raise ValueError('artifact-depth')
        if isinstance(item, dict):
            pending.extend((child, depth + 1) for child in item.values())
            pending.extend((key, depth + 1) for key in item)
        elif isinstance(item, list):
            pending.extend((child, depth + 1) for child in item)
        elif isinstance(item, str):
            item.encode('utf-8', errors='strict')
        elif isinstance(item, float):
            import math
            if not math.isfinite(item):
                raise ValueError('nonfinite')
    return value


def canonical_bytes(value):
    return (json.dumps(value, sort_keys=True, ensure_ascii=True,
                       allow_nan=False, separators=(',', ':')) + '\n').encode('ascii')


def artifact_write(path, value):
    data = canonical_bytes(value)
    if len(data) > MAX_ARTIFACT:
        raise ValueError('artifact-limit')
    with Path(path).open('xb') as stream:
        stream.write(data)


def product_identity(catalog_path):
    import hashlib
    import os
    import subprocess
    env = {key: os.environ[key] for key in ('PATH', 'HOME') if key in os.environ}
    def git(*args):
        result = subprocess.run(['/usr/bin/git', '-c', 'core.fsmonitor=false', *args],
                                cwd=ROOT, env=env, stdin=subprocess.DEVNULL,
                                capture_output=True, timeout=5, check=True)
        if len(result.stdout) > MAX_ARTIFACT:
            raise ValueError('identity-limit')
        return result.stdout
    commit = git('rev-parse', '--verify', 'HEAD').decode('ascii').strip()
    if not re.fullmatch(r'[0-9a-f]{40,64}', commit):
        raise ValueError('source-identity')
    paths = git('ls-files', '-z', '--', 'Cargo.toml', 'Cargo.lock', 'build.rs',
                'rust-toolchain.toml', 'src', 'tests', 'scripts').split(b'\0')
    names = sorted(p.decode('utf-8') for p in paths if p)
    if not names or len(names) > 10_000:
        raise ValueError('source-files')
    hashes = {}
    total = 0
    for name in names:
        path = ROOT / name
        if path.is_symlink() or any(p.is_symlink() for p in path.parents if p != ROOT):
            raise ValueError('source-symlink')
        data = bounded_bytes(path, 8 * 1024 * 1024)
        total += len(data)
        if total > MAX_LOG:
            raise ValueError('source-bytes')
        hashes[name] = hashlib.sha256(data).hexdigest()
    digest = lambda p: hashlib.sha256(bounded_bytes(p, MAX_ARTIFACT)).hexdigest()
    return {'git_commit': commit, 'local_inputs_sha256': hashlib.sha256(canonical_bytes(hashes)).hexdigest(),
            'lock_sha256': hashes['Cargo.lock'], 'toolchain_sha256': hashes['rust-toolchain.toml'],
            'catalog_sha256': digest(catalog_path), 'features': [],
            'fixture_sha256': hashes['tests/fixtures/jev-tls/provider_server.py'],
            'remote_inputs_verified': False}


def product_artifact_start(catalog_path, directory):
    import os
    import time
    directory = Path(directory)
    if (directory.is_symlink() or directory.stat().st_mode & 0o077
            or directory.stat().st_uid != os.geteuid()):
        raise ValueError('artifact-directory-not-private')
    os.umask(0o077)
    artifact_write(directory / 'started.json', {'schema_version': 1,
                   'monotonic_ns': time.monotonic_ns(), 'identity': product_identity(catalog_path)})


def product_artifact_record(catalog_path, directory, exit_code):
    import hashlib
    import os
    import time
    os.umask(0o077)
    directory = Path(directory)
    if (directory.is_symlink() or directory.stat().st_mode & 0o077
            or directory.stat().st_uid != os.geteuid()):
        raise ValueError('artifact-directory-not-private')
    started = artifact_json(bounded_bytes(directory / 'started.json', MAX_ARTIFACT))
    elapsed = time.monotonic_ns() - started['monotonic_ns']
    if elapsed < 0:
        raise ValueError('monotonic-clock')
    catalog = load_catalog(catalog_path)
    raw_log = bounded_bytes(directory / 'tests.log', MAX_LOG)
    raw_remote = bounded_bytes(directory / 'remote-result.json', MAX_ARTIFACT)
    # This fixed synthetic marker is only a detector control, never a credential.
    leak_free = CANARY not in raw_log and CANARY not in raw_remote
    text = raw_log.decode('utf-8', errors='replace')
    records = evaluate(catalog, text)
    results, _, _ = parse_log(text)
    try:
        remote = artifact_json(raw_remote)
    except (ValueError, UnicodeError, RecursionError):
        remote = {}
    if not isinstance(remote, dict):
        remote = {}
    admission = remote.get('admission') or {}
    if not isinstance(admission, dict):
        admission = {}
    executed = (exit_code == 0 and remote.get('location') == 'remote'
                and remote.get('outcome') == 'completed' and remote.get('remote_exit_code') == 0
                and admission.get('strict_remote') is True
                and admission.get('terminal_acknowledged') is True)
    identity_stable = started['identity'] == product_identity(catalog_path)
    # Safe projection of a receipt emitted AFTER the real CLI assertions. No
    # provider body, absolute artifact path or inherited environment is copied.
    controls = []
    for line in text.splitlines():
        if '"case":"prespecified-wire-variants"' not in line:
            continue
        if len(line.encode('utf-8')) > MAX_ARTIFACT:
            raise ValueError('control-limit')
        try:
            receipt = artifact_json(line[line.index('{'):].encode('utf-8'))
        except (ValueError, RecursionError):
            continue
        if receipt.get('case') != 'prespecified-wire-variants':
            continue
        control = {key: receipt.get(key) for key in (
            'scenario', 'attempt_cap', 'compiled_source', 'binary_blake3', 'platform',
            'computation_digest', 'numeric_build_digest', 'elapsed_ns',
            'http_attempts', 'unknown_usage_attempts', 'fault_reached', 'leak_detector_checked')}
        # Never copy unvalidated text from an arbitrary test diagnostic.
        if not (control['scenario'] in ('useful', 'stale-renamed-option')
                and control['attempt_cap'] in ('16', '12', '4')
                and isinstance(control['compiled_source'], str)
                and re.fullmatch(r'[0-9a-f]{40,64}(?:-overlay-[0-9a-f]{64})?', control['compiled_source'])
                and all(isinstance(control[k], str) and re.fullmatch(r'[0-9a-f]{64}', control[k])
                        for k in ('binary_blake3', 'computation_digest', 'numeric_build_digest'))
                and isinstance(control['platform'], str) and re.fullmatch(r'[a-z0-9_-]{1,64}', control['platform'])
                and all(type(control[k]) is int and 0 <= control[k] <= 7_200_000_000_000
                        for k in ('elapsed_ns', 'http_attempts', 'unknown_usage_attempts'))
                and all(type(control[k]) is bool for k in ('fault_reached', 'leak_detector_checked'))):
            continue
        controls.append(control)
        if len(controls) > 16:
            raise ValueError('control-limit')
    binary = None
    controls_complete = True
    if catalog['suite'] == 'replay':
        expected = [('useful', '16', 12, 0, False), ('stale-renamed-option', '16', 11, 1, True),
                    ('useful', '12', 10, 0, False), ('useful', '4', 2, 0, False)]
        observed = [(r['scenario'], r['attempt_cap'], r['http_attempts'],
                     r['unknown_usage_attempts'], r['fault_reached']) for r in controls]
        binaries = {r['binary_blake3'] for r in controls}
        platforms = {r['platform'] for r in controls}
        controls_complete = (observed == expected and len(binaries) == 1 and len(platforms) == 1
                             and all(type(r['elapsed_ns']) is int and r['elapsed_ns'] > 0
                                     and r['leak_detector_checked'] is True
                                     and isinstance(r['compiled_source'], str)
                                     and (r['compiled_source'] == started['identity']['git_commit']
                                          or r['compiled_source'].startswith(started['identity']['git_commit'] + '-overlay-'))
                                     and all(isinstance(r[key], str) and re.fullmatch(r'[0-9a-f]{64}', r[key])
                                             for key in ('binary_blake3', 'computation_digest', 'numeric_build_digest'))
                                     and isinstance(r['platform'], str) and re.fullmatch(r'[a-z0-9_-]{1,64}', r['platform'])
                                     for r in controls))
        if controls_complete:
            binary = {'blake3': next(iter(binaries)), 'platform': next(iter(platforms)),
                      'role': 'actual-cargo-built-sr'}
    events = [{'schema_version': 1, 'suite': catalog['suite'], 'kind': 'run',
               'step': 'remote-product-tests', 'expected': {'exit': 0, 'remote': True},
               'actual': {'exit': exit_code, 'remote_completed': executed},
               'elapsed_ns': elapsed, 'identity': started['identity'], 'binary': binary,
               'fault_reached': None, 'raw_log_sha256': hashlib.sha256(raw_log).hexdigest()}]
    for case, record in zip(catalog['cases'], records, strict=True):
        refs = case['tests'] if case['tests'] != ALL else []
        observed = {ref: results.get(ref.partition('::')[2], 'missing') for ref in refs}
        for assertion in case['assertions']:
            events.append({'schema_version': 1, 'suite': catalog['suite'], 'case': case['id'],
                           'step': 'mapped-rust-controls', 'assertion': assertion,
                           'expected': {'status': 'passed', 'complete_run': True},
                           'actual': {'status': record['status'], 'tests': observed},
                           'status': record['status'], 'elapsed_ns': None, 'fault_reached': None})
    events.extend({'schema_version': 1, 'suite': catalog['suite'], 'case': 'prespecified-wire-variants',
                   'step': 'actual-cli-tls-control', 'control': control} for control in controls)
    encoded = b''.join(canonical_bytes(event) for event in events)
    if len(encoded) > MAX_ARTIFACT:
        raise ValueError('artifact-limit')
    with (directory / 'events.jsonl').open('xb') as stream:
        stream.write(encoded)
    passed = sum(record['status'] == 'passed' for record in records)
    accepted = (executed and identity_stable and leak_free and controls_complete
                and passed == len(records) and bool(records))
    summary = {'schema_version': 1, 'suite': catalog['suite'], 'tier': 'rust-product-integration',
               'status': 'passed' if accepted else 'failed', 'cases': len(records),
               'cases_passed': passed, 'assertions': sum(len(c['assertions']) for c in catalog['cases']),
               'identity': started['identity'], 'binary': binary, 'identity_stable': identity_stable,
               'leak_free': leak_free, 'controls_complete': controls_complete, 'elapsed_ns': elapsed,
               'exit': exit_code, 'events_sha256': hashlib.sha256(encoded).hexdigest(),
               'quality_gate': 'not-established', 'live_provider': False, 'native_harness': False}
    artifact_write(directory / 'summary.json', summary)
    return 0 if accepted else 1


def load_catalog(path):
    catalog = artifact_json(bounded_bytes(path, MAX_ARTIFACT))
    if catalog.get("schema_version") != 1 or catalog.get("tier") != "rust-product-integration":
        raise ValueError("unsupported catalog")
    ids = [case["id"] for case in catalog["cases"]]
    if len(ids) != len(set(ids)) or not catalog["targets"]:
        raise ValueError("duplicate case IDs or no targets")
    return catalog


def defined_tests(target):
    source = ROOT / "tests" / f"{target}.rs"
    return set(TEST_FN.findall(source.read_text())) if source.is_file() else None


def ignored_tests(target):
    source = ROOT / "tests" / f"{target}.rs"
    return set(IGNORED_FN.findall(source.read_text())) if source.is_file() else set()


def allowed_ignored(catalog):
    """Declared opt-in tests that a complete run may report as ignored."""
    return {entry["test"].partition("::")[2] for entry in catalog.get("allowed_ignored", [])}


def check(catalog, matrix_path=MATRIX):
    """Return a list of problems; empty means the catalog is consistent."""
    problems = []
    defined = {}
    for target in catalog["targets"]:
        tests = defined_tests(target)
        if tests is None:
            problems.append(f"missing target source tests/{target}.rs")
            tests = set()
        defined[target] = tests
    # Logs identify tests by unqualified name. A collision would make distinct
    # target results indistinguishable, even when no named case maps that test.
    for name in sorted(set().union(*defined.values())):
        owners = [target for target, tests in defined.items() if name in tests]
        if len(owners) > 1:
            problems.append(f"{name} is ambiguous across {owners}")
    for case in catalog["cases"]:
        if case["tests"] == ALL:
            continue
        if not case["tests"]:
            problems.append(f"{case['id']}: no tests")
        for reference in case["tests"]:
            target, _, name = reference.partition("::")
            if target not in defined:
                problems.append(f"{case['id']}: {target} is not a suite target")
            elif name not in defined[target]:
                problems.append(f"{case['id']}: {reference} is not a #[test] fn")
    for entry in catalog.get("allowed_ignored", []):
        target, _, name = entry["test"].partition("::")
        if not entry.get("reason"):
            problems.append(f"allowed ignored {entry['test']} needs a reason")
        if target not in defined or name not in ignored_tests(target):
            problems.append(f"allowed ignored {entry['test']} is not an #[ignore] test")
        if any(entry["test"] in c["tests"] for c in catalog["cases"] if c["tests"] != ALL):
            problems.append(f"allowed ignored {entry['test']} cannot establish a case")
    declared_ignored = {entry["test"] for entry in catalog.get("allowed_ignored", [])}
    for target in defined:
        for name in sorted(ignored_tests(target)):
            reference = f"{target}::{name}"
            if reference not in declared_ignored:
                problems.append(f"ignored test {reference} needs an allowed_ignored declaration")
    matrix = tomllib.loads(Path(matrix_path).read_text())
    cases = {case["id"]: set(case["assertions"]) for case in catalog["cases"]}
    required = set()
    for row in matrix["boundaries"]:
        if row["e2e_suite"] != catalog["suite"]:
            continue
        for case in row["e2e_cases"]:
            required.add(case)
            if case not in cases:
                problems.append(f"{row['id']}: case {case} has no catalog entry")
            elif not set(row["assertion_ids"]) <= cases[case]:
                problems.append(f"{row['id']}: case {case} lacks its assertions")
    for case in cases.keys() - required:
        problems.append(f"catalog case {case} is not in the matrix")
    return problems


def parse_log(text):
    results = {}
    summaries = []
    nested = 0
    for line in text.splitlines():
        line = line.rstrip()
        match = RESULT.match(line)
        if match:
            name, status = match.groups()
            if RANK[status] >= RANK.get(results.get(name, "ok"), 0):
                results[name] = status
            continue
        match = SUMMARY.match(line)
        if match:
            summary = tuple([match[1]] + [int(x) for x in match.groups()[1:]])
            # A filtered summary comes from a test's own `--exact` child run.
            if summary[5] > 0:
                nested += 1
            else:
                summaries.append(summary)
    return results, summaries, nested


def evaluate(catalog, text):
    results, summaries, nested = parse_log(text)
    ignored = {name for name, status in results.items() if status == "ignored"}
    passed = {name for name, status in results.items() if status == "ok"}
    complete = (
        len(summaries) == len(catalog["targets"])
        and all(s[0] == "ok" and s[1] > 0 and s[2] == 0 and s[4] == 0 for s in summaries)
        # Summary-only or partially lost logs must never certify a whole run.
        # Exact child runs repeat parent names and do not add to this count.
        and sum(s[1] for s in summaries) == len(passed)
        # Only declared opt-in tests may be ignored, and the counts must agree.
        and ignored <= allowed_ignored(catalog)
        and sum(s[3] for s in summaries) == len(ignored)
        and not any(status == "FAILED" for status in results.values())
    )
    records = []
    for case in catalog["cases"]:
        if case["tests"] == ALL:
            status = "passed" if complete else "failed"
            detail = {
                "targets": len(summaries),
                "passed": sum(s[1] for s in summaries),
                "failed": sum(s[2] for s in summaries),
                "ignored": sorted(ignored),
                "nested_child_runs": nested,
            }
        else:
            names = [reference.partition("::")[2] for reference in case["tests"]]
            missing = [n for n in names if n not in results]
            failed = [n for n in names if results.get(n) in ("FAILED", "ignored")]
            status = "failed" if failed else "missing" if missing else "passed"
            if status == "passed" and not complete:
                status = "failed"
            detail = {"tests": len(names), "missing": missing, "failed": failed}
        records.append({
            "schema_version": 1,
            "suite": catalog["suite"],
            "case": case["id"],
            "assertions": case["assertions"],
            "status": status,
            **detail,
        })
    return records


def main(argv):
    if len(argv) == 4 and argv[1] == "snapshot":
        product_artifact_start(argv[2], argv[3])
        return 0
    if len(argv) == 5 and argv[1] == "record":
        return product_artifact_record(argv[2], argv[3], int(argv[4]))
    if len(argv) == 3 and argv[1] == "check":
        problems = check(load_catalog(argv[2]))
        for problem in problems:
            print(problem, file=sys.stderr)
        return 1 if problems else 0
    if len(argv) == 3 and argv[1] == "targets":
        for target in load_catalog(argv[2])["targets"]:
            print(target)
        return 0
    if len(argv) == 4 and argv[1] == "evaluate":
        catalog = load_catalog(argv[2])
        records = evaluate(catalog, bounded_bytes(argv[3], MAX_LOG).decode("utf-8", errors="replace"))
        for record in records:
            print(json.dumps(record, sort_keys=True))
        passed = sum(r["status"] == "passed" for r in records)
        print(json.dumps({"schema_version": 1, "suite": catalog["suite"],
                          "cases": len(records), "passed": passed,
                          "status": "passed" if passed == len(records) else "failed"}))
        return 0 if passed == len(records) else 1
    print("usage: product_cases.py check CATALOG | targets CATALOG | evaluate CATALOG LOG | snapshot CATALOG DIR | record CATALOG DIR EXIT",
          file=sys.stderr)
    return 2


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv))
    except (ValueError, OSError, TypeError, UnicodeError, RecursionError, KeyError):
        print("invalid or incomplete product evidence", file=sys.stderr)
        sys.exit(2)
