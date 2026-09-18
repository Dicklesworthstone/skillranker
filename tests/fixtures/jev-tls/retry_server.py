"""Bounded real TLS sequences; only public synthetic test credentials/data."""

import json
import pathlib
import socket
import ssl
import sys
import time


def emit(value):
    print(json.dumps(value), flush=True)


root = pathlib.Path(__file__).parent
steps = json.loads(sys.argv[1])
assert 1 <= len(steps) <= 5
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(root / "server.pem", root / "server.key")
context.set_alpn_protocols(["http/1.1"])
listener = socket.socket()
listener.bind(("127.0.0.1", 0))
listener.listen(2)
listener.settimeout(5)
emit({"port": listener.getsockname()[1]})
times = []
closed = []
request_bytes = 0
for step in steps:
    connection, _ = listener.accept()
    connection.settimeout(5)
    stream = context.wrap_socket(connection, server_side=True)
    raw = b""
    while b"\r\n\r\n" not in raw:
        block = stream.recv(4096)
        assert block
        raw += block
        assert len(raw) <= 128 * 1024
    header, body = raw.split(b"\r\n\r\n", 1)
    lines = header.decode("ascii").split("\r\n")
    fields = dict(line.split(":", 1) for line in lines[1:])
    fields = {key.lower(): value.strip() for key, value in fields.items()}
    size = int(fields["content-length"])
    assert 0 <= size <= 96 * 1024
    while len(body) < size:
        block = stream.recv(min(4096, size - len(body)))
        assert block
        body += block
    assert len(body) == size
    assert lines[0] == "POST /v1/systemone HTTP/1.1"
    # ubs:ignore -- public synthetic canary, not a credential or authentication service.
    assert fields["authorization"] == "Bearer synthetic-retry-canary"
    assert fields["connection"].lower() == "close"
    assert json.loads(body)["state"] == "synthetic retry probe"
    times.append(time.monotonic_ns())
    request_bytes += size
    mode, _, argument = step.partition(":")
    status = 200 if mode in ("ok", "malformed", "slow", "cancel") else int(mode)
    response = json.dumps({
        "model": argument or "synthetic-revision-1",
        "answers": {"fit": {"type": "noul", "noul": 0.75}},
        "usage": {"input_tokens": 3, "output_tokens": 2},
    }).encode()
    if mode == "malformed":
        response = b'{"private-provider-body synthetic-retry-canary":'
    elif status != 200:
        response = b"private-provider-body synthetic-retry-canary"
    extra = ""
    if status != 200 and argument:
        assert "\r" not in argument and "\n" not in argument
        if argument == "duplicate":
            extra = "Retry-After: 0\r\nRetry-After: 1\r\n"
        else:
            extra = f"Retry-After: {argument}\r\n"
    stream.sendall((f"HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\n{extra}Content-Length: {len(response)}\r\nConnection: close\r\n\r\n").encode())
    if mode == "cancel":
        emit({"body_pending": True})
    if mode not in ("slow", "cancel"):
        stream.sendall(response)
    try:
        closed.append(stream.recv(1) == b"")
    except (ssl.SSLError, ConnectionResetError, BrokenPipeError):
        closed.append(True)
    stream.close()

# Catch an unexpected fifth/repeated attempt independently of Rust counters.
listener.settimeout(0.25)
try:
    unexpected, _ = listener.accept()
    unexpected.close()
    extra_connections = 1
except TimeoutError:
    extra_connections = 0
listener.close()
emit({"requests": len(times), "received_ns": times, "closed": closed,
      "extra_connections": extra_connections, "request_bytes": request_bytes})
