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


def read_request(reader, protocol, capture=False):
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
    if not capture:
        assert payload["state"] == "synthetic context"
        assert payload["questions"]["fit"]["type"] == "noul"
    else:
        assert isinstance(payload["state"], (str, dict, list))
        assert isinstance(payload["questions"], dict) and payload["questions"]
    return headers.get("connection", "").lower(), body, payload


def response(step, protocol, payload=None):
    result = {
        "model": "jev-synthetic-revision",
        "answers": {"fit": {"type": "noul", "noul": 0.75}},
        "usage": {"input_tokens": 12, "output_tokens": 7},
    }
    if step.startswith("capture"):
        # Synthetic contract answers bind every real question/option ID. This
        # peer proves bytes and TLS behavior, never recommendation quality.
        answers = {}
        for key, question in payload["questions"].items():
            if question["type"] == "noul":
                value = 0.1 if key == "gate::context_suffices" else 0.8
                answers[key] = {"type": "noul", "noul": value}
            else:
                assert question["type"] == "choice"
                options = list(question["criteria"])
                favored = next((option for option in options if option != "__none__"), options[0])
                rest = [option for option in options if option != favored]
                probabilities = {option: 0.2 / len(rest) for option in rest}
                probabilities[favored] = 0.8 if rest else 1.0
                answers[key] = {"type": "choice", "choice": favored,
                                "probabilities": probabilities, "confidence": 0.8}
        result["answers"] = answers
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
        "captured_requests": [],
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
                    captured = read_request(reader, protocol, steps[index].startswith("capture"))
                    if captured is None:
                        report["closed"].append(True)
                        break
                    connection, request_body, payload = captured
                    report["requests"] += 1
                    report["valid_requests"] += 1
                    report["connection_headers"].append(connection)
                    step = steps[index]
                    if step.startswith("capture"):
                        report["captured_requests"].append(request_body.decode("utf-8"))
                    index += 1
                    status, headers, body = response(step, protocol, payload)
                    keep = step in ("reuse", "capture-reuse") and index < len(steps)
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
