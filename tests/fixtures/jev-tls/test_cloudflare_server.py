#!/usr/bin/env python3
"""Self-check the loopback peer; these tests do NOT execute the Rust transport."""

from contextlib import contextmanager
import http.client
import json
from pathlib import Path
import ssl
import subprocess
import sys
import unittest

ROOT = Path(__file__).resolve().parent
ACCOUNT = "0123456789abcdef0123456789abcdef"
TOKEN = "synthetic-cloudflare-transport-token"


@contextmanager
def peer(steps, protocol="native"):
    child = subprocess.Popen(
        [sys.executable, str(ROOT / "cloudflare_server.py"), json.dumps(steps), protocol],
        env={},
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        hello = json.loads(child.stdout.readline())
        yield child, hello["port"]
    finally:
        if child.poll() is None:
            child.kill()
        child.wait(timeout=5)
        child.stdout.close()
        child.stderr.close()


def connect(port, host="localhost", trusted=True):
    context = ssl.create_default_context(cafile=str(ROOT / "ca.pem") if trusted else None)
    return http.client.HTTPSConnection(host, port, context=context, timeout=5)


def send(connection, protocol="native", keep=False):
    payload = {
        "state": "synthetic context",
        "questions": {"fit": {"type": "noul", "instructions": "Synthetic score"}},
    }
    if protocol == "native":
        document = {"model": "typesafe/jev", "input": payload}
        path = f"/client/v4/accounts/{ACCOUNT}/ai/run"
    else:
        document = {"model": "jev-latest", **payload}
        path = "/v1/systemone"
    connection.request(
        "POST", path, json.dumps(document).encode(),
        {
            "Authorization": f"Bearer {TOKEN}",
            "Content-Type": "application/json",
            "Accept-Encoding": "identity",
            "Connection": "keep-alive" if keep else "close",
        },
    )
    return connection.getresponse()


def report(child):
    line = child.stdout.readline()
    code = child.wait(timeout=5)
    if code != 0:
        raise AssertionError(f"fixture exited {code}: {child.stderr.read()}")
    return json.loads(line)


class NativePeerTests(unittest.TestCase):
    def assert_report(self, output, requests, connections):
        self.assertEqual(output["requests"], requests)
        self.assertEqual(output["valid_requests"], requests)
        self.assertEqual(output["connections"], connections)
        self.assertEqual(output["extra_connections"], 0)
        self.assertTrue(all(output["closed"]))

    def test_native_response_shapes(self):
        for shape in ("ok", "flat", "top"):
            with self.subTest(shape=shape), peer([shape]) as (child, port):
                connection = connect(port)
                try:
                    response = send(connection)
                    self.assertEqual(response.status, 200)
                    body = json.loads(response.read())
                    inner = body if shape == "top" else body["result"]
                    if shape == "ok":
                        inner = inner["result"]
                    self.assertIs(body["success"], True)
                    self.assertEqual(inner["model"], "jev-synthetic-revision")
                    self.assertEqual(inner["usage"], {"input_tokens": 12, "output_tokens": 7})
                finally:
                    connection.close()
                self.assert_report(report(child), 1, 1)

    def test_capture_preserves_exact_synthetic_unicode_body_for_both_protocols(self):
        for protocol in ("native", "typesafe"):
            with self.subTest(protocol=protocol), peer(["capture"], protocol) as (child, port):
                payload = {"state":"é界 synthetic", "questions":{
                    "which":{"type":"choice", "instructions":"choose",
                             "criteria":{"s_alpha":"alpha", "__none__":"none"}},
                    "gate::context_suffices":{"type":"noul", "instructions":"sufficient?"}
                }}
                document = ({"model":"typesafe/jev", "input":payload} if protocol == "native"
                            else {"model":"jev-latest", **payload})
                wire = json.dumps(document, ensure_ascii=False, separators=(",", ":")).encode()
                connection = connect(port)
                try:
                    path = f"/client/v4/accounts/{ACCOUNT}/ai/run" if protocol == "native" else "/v1/systemone"
                    connection.request("POST", path, wire, {
                        "Authorization":f"Bearer {TOKEN}", "Content-Type":"application/json",
                        "Accept-Encoding":"identity", "Connection":"close"
                    })
                    answer = connection.getresponse()
                    self.assertEqual(answer.status, 200)
                    body = json.loads(answer.read())
                    result = body["result"]["result"] if protocol == "native" else body
                    self.assertEqual(result["answers"]["which"]["choice"], "s_alpha")
                    self.assertEqual(result["answers"]["gate::context_suffices"]["noul"], 0.1)
                finally:
                    connection.close()
                output = report(child)
                self.assert_report(output, 1, 1)
                self.assertEqual(output["captured_requests"], [wire.decode()])

    def test_both_protocols_reuse_a_verified_tls_connection(self):
        for protocol in ("typesafe", "native"):
            with self.subTest(protocol=protocol), peer(["reuse", "ok"], protocol) as (child, port):
                connection = connect(port)
                try:
                    first = send(connection, protocol, keep=True)
                    self.assertEqual(first.status, 200)
                    first.read()
                    second = send(connection, protocol)
                    self.assertEqual(second.status, 200)
                    second.read()
                finally:
                    connection.close()
                output = report(child)
                self.assert_report(output, 2, 1)
                self.assertEqual(output["connection_headers"], ["keep-alive", "close"])
                self.assertEqual(output["closed"], [True])

    def test_retry_sequence_requires_two_connections(self):
        with peer(["429:0", "ok"]) as (child, port):
            for status in (429, 200):
                connection = connect(port)
                try:
                    response = send(connection)
                    self.assertEqual(response.status, status)
                    if status == 429:
                        self.assertEqual(response.getheader("Retry-After"), "0")
                    response.read()
                finally:
                    connection.close()
            self.assert_report(report(child), 2, 2)

    def test_malformed_and_incomplete_responses_are_not_repaired_by_the_fixture(self):
        for step in ("missing-usage", "partial-usage", "missing-model", "duplicate", "malformed", "unsuccessful"):
            with self.subTest(step=step), peer([step]) as (child, port):
                connection = connect(port)
                try:
                    response = send(connection)
                    body = response.read()
                    if step == "malformed":
                        self.assertEqual(body, b"} malformed {")
                    elif step == "duplicate":
                        self.assertIn(b'"success":false,"success":true', body)
                    else:
                        envelope = json.loads(body)
                        inner = envelope["result"]["result"]
                        if step == "missing-usage":
                            self.assertNotIn("usage", inner)
                        elif step == "partial-usage":
                            self.assertNotIn("output_tokens", inner["usage"])
                        elif step == "missing-model":
                            self.assertNotIn("model", inner)
                        else:
                            self.assertIs(envelope["success"], False)
                finally:
                    connection.close()
                self.assert_report(report(child), 1, 1)

    def test_response_header_and_size_failure_modes(self):
        for step in ("wrong-type", "missing-type", "duplicate-type", "encoding", "duplicate-encoding", "oversized"):
            with self.subTest(step=step), peer([step]) as (child, port):
                connection = connect(port)
                try:
                    response = send(connection)
                    body = response.read()
                    types = response.headers.get_all("Content-Type", [])
                    encodings = response.headers.get_all("Content-Encoding", [])
                    if step == "wrong-type":
                        self.assertEqual(types, ["text/plain"])
                    elif step == "missing-type":
                        self.assertEqual(types, [])
                    elif step == "duplicate-type":
                        self.assertEqual(types, ["application/json", "application/json"])
                    elif step == "encoding":
                        self.assertEqual(encodings, ["gzip"])
                    elif step == "duplicate-encoding":
                        self.assertEqual(encodings, ["identity", "identity"])
                    else:
                        self.assertGreater(len(body), 2 * 1024 * 1024)
                finally:
                    connection.close()
                self.assert_report(report(child), 1, 1)

    def test_status_and_redirect_metadata(self):
        for status in (302, 401, 503):
            with self.subTest(status=status), peer([f"status:{status}"]) as (child, port):
                connection = connect(port)
                try:
                    response = send(connection)
                    self.assertEqual(response.status, status)
                    self.assertEqual(response.getheader("Retry-After"), "3")
                    if status == 302:
                        self.assertEqual(response.getheader("Location"), "/must-not-follow")
                    response.read()
                finally:
                    connection.close()
                self.assert_report(report(child), 1, 1)

    def test_tls_verification_rejects_unknown_ca_and_wrong_hostname(self):
        for host, trusted in (("localhost", False), ("127.0.0.1", True)):
            with self.subTest(host=host, trusted=trusted), peer(["ok"]) as (child, port):
                connection = connect(port, host, trusted)
                try:
                    with self.assertRaises(ssl.SSLCertVerificationError):
                        send(connection)
                finally:
                    connection.close()
                self.assert_report(report(child), 0, 1)

    def test_stalled_response_reports_actual_start_and_observes_close(self):
        with peer(["stall"]) as (child, port):
            connection = connect(port)
            try:
                response = send(connection)
                self.assertEqual(json.loads(child.stdout.readline()), {"body_started": True})
                self.assertEqual(response.read(1), b"{")
                self.assertGreater(int(response.getheader("Content-Length")), 1)
                response.close()
            finally:
                connection.close()
            output = report(child)
            self.assert_report(output, 1, 1)
            self.assertEqual(output["closed"], [True])


if __name__ == "__main__":
    unittest.main(verbosity=2)
