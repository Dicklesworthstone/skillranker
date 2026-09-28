#!/usr/bin/env python3
"""Bounded loopback TLS peer for native Jev transport tests; synthetic data only."""

import json
from pathlib import Path
import socket
import signal
import ssl
import sys

ACCOUNT = "0123456789abcdef0123456789abcdef"
TOKEN = "synthetic-cloudflare-transport-token"
MAX_REQUEST = 96 * 1024
MAX_HEADERS = 32 * 1024


def read_request(reader, protocol):
    line = reader.readline(MAX_HEADERS + 1)
    if not line:
        return None
    total = len(line)
    expected = (
        f"/client/v4/accounts/{ACCOUNT}/ai/run"
        if protocol == "native" else "/v1/systemone"
    )
    assert line == f"POST {expected} HTTP/1.1\r\n".encode()
    headers = {}
    while True:
        line = reader.readline(MAX_HEADERS + 1)
        total += len(line)
        assert total <= MAX_HEADERS and line
        if line == b"\r\n":
            break
        name, value = line.decode("ascii").split(":", 1)
        name = name.lower()
        assert name not in headers
        headers[name] = value.strip()
    assert headers.get("authorization") == f"Bearer {TOKEN}"
    assert headers.get("content-type") == "application/json"
    assert headers.get("accept-encoding") == "identity"
    size = int(headers["content-length"])
    assert 0 < size <= MAX_REQUEST
    body = reader.read(size)
    assert len(body) == size
    document = json.loads(body)
    if protocol == "native":
        assert set(document) == {"model", "input"}
        assert document["model"] == "typesafe/jev"
        assert set(document["input"]) == {"state", "questions"}
        payload = document["input"]
    else:
        assert set(document) == {"model", "state", "questions"}
        assert document["model"] == "jev-latest"
        payload = document
    assert payload["state"] == "synthetic context"
    assert payload["questions"]["fit"]["type"] == "noul"
    return headers.get("connection", "").lower()


def response(step, protocol):
    result = {
        "model": "jev-synthetic-revision",
        "answers": {"fit": {"type": "noul", "noul": 0.75}},
        "usage": {"input_tokens": 12, "output_tokens": 7},
    }
    status = 200
    extra = []
    if step.startswith("status:"):
        status = int(step.split(":")[1])
        extra = [("Retry-After", "3")]
        if 300 <= status < 400:
            extra.append(("Location", "/must-not-follow"))
    elif step == "429:0":
        status = 429
        extra = [("Retry-After", "0")]
    elif step == "missing-usage":
        del result["usage"]
    elif step == "partial-usage":
        del result["usage"]["output_tokens"]
    elif step == "missing-model":
        del result["model"]
    envelope = {"success": True, "result": {"result": result}}
    if protocol == "typesafe":
        envelope = result
    elif step == "flat":
        envelope = {"success": True, "result": result}
    elif step == "top":
        envelope = {"success": True, **result}
    elif step == "unsuccessful":
        envelope["success"] = False
    elif step == "oversized":
        envelope["metadata"] = "x" * (2 * 1024 * 1024)
    body = json.dumps(envelope, separators=(",", ":")).encode()
    if step == "duplicate":
        body = body.replace(b'"success":true', b'"success":false,"success":true', 1)
    elif step == "malformed":
        body = b"} malformed {"
    headers = [("Content-Type", "application/json")]
    if step == "wrong-type":
        headers = [("Content-Type", "text/plain")]
    elif step == "missing-type":
        headers = []
    elif step == "duplicate-type":
        headers *= 2
    elif step == "encoding":
        headers.append(("Content-Encoding", "gzip"))
    elif step == "duplicate-encoding":
        headers.extend([("Content-Encoding", "identity")] * 2)
    headers.extend(extra)
    return status, headers, body


def main():
    steps = json.loads(sys.argv[1])
    protocol = sys.argv[2] if len(sys.argv) > 2 else "native"
    assert isinstance(steps, list) and 1 <= len(steps) <= 4
    assert all(isinstance(step, str) and len(step) <= 32 for step in steps)
    assert protocol in ("native", "typesafe")
    root = Path(__file__).resolve().parent
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(root / "server.pem", root / "server.key")
    report = {
        "requests": 0, "connections": 0, "extra_connections": 0,
        "closed": [], "connection_headers": [], "valid_requests": 0,
    }
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        listener.listen(4)
        listener.settimeout(10)
        print(json.dumps({"port": listener.getsockname()[1]}), flush=True)
        index = 0
        while index < len(steps):
            raw, _ = listener.accept()
            report["connections"] += 1
            raw.settimeout(10)
            try:
                stream = context.wrap_socket(raw, server_side=True)
            except (ssl.SSLError, ConnectionError):
                raw.close()
                break
            with stream, stream.makefile("rb") as reader:
                while index < len(steps):
                    connection = read_request(reader, protocol)
                    if connection is None:
                        report["closed"].append(True)
                        break
                    report["requests"] += 1
                    report["valid_requests"] += 1
                    report["connection_headers"].append(connection)
                    step = steps[index]
                    index += 1
                    status, headers, body = response(step, protocol)
                    keep = step == "reuse" and index < len(steps)
                    length = len(body) if step != "stall" else len(body) + 1024
                    headers.extend([
                        ("Content-Length", str(length)),
                        ("Connection", "keep-alive" if keep else "close"),
                    ])
                    head = f"HTTP/1.1 {status} Fixture\r\n" + "".join(
                        f"{name}: {value}\r\n" for name, value in headers
                    ) + "\r\n"
                    try:
                        stream.sendall(head.encode("ascii"))
                        stream.sendall(body if step != "stall" else b"{")
                        if step == "stall":
                            print(json.dumps({"body_started": True}), flush=True)
                        if not keep:
                            report["closed"].append(reader.read(1) == b"")
                            break
                    except (ssl.SSLError, ConnectionError):
                        report["closed"].append(True)
                        break
        listener.settimeout(0.1)
        try:
            while True:
                extra, _ = listener.accept()
                extra.close()
                report["extra_connections"] += 1
        except socket.timeout:
            pass
    print(json.dumps(report), flush=True)


if __name__ == "__main__":
    try:
        signal.alarm(30)
        main()
    except (AssertionError, OSError, ValueError, KeyError, TypeError):
        # Deliberately omit request bytes, headers and parser exception details.
        print("synthetic TLS fixture contract failed", file=sys.stderr, flush=True)
        sys.exit(2)
