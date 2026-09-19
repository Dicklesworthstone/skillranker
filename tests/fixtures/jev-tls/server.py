"""Real loopback TLS server for the Rust transport tests; synthetic data only."""

import gzip
import json
import pathlib
import socket
import ssl
import sys


def emit(value):
    print(json.dumps(value), flush=True)


root = pathlib.Path(__file__).parent
mode = sys.argv[1]
listener = socket.socket()
listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
listener.bind(("127.0.0.1", 0))
listener.listen(2)
listener.settimeout(5)
emit({"port": listener.getsockname()[1]})
connection, _ = listener.accept()
connection.settimeout(5)
listener.close()

if mode == "slow-handshake":
    count = 0
    while True:
        block = connection.recv(65536)
        if not block:
            break
        count += len(block)
    emit({"closed": True, "handshake_bytes": count, "requests": 0})
    connection.close()
    sys.exit(0)

context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(root / "server.pem", root / "server.key")
context.set_alpn_protocols(["http/1.1"])
try:
    stream = context.wrap_socket(connection, server_side=True)
except ssl.SSLError:
    emit({"handshake_rejected": True, "requests": 0})
    connection.close()
    sys.exit(0)

raw = b""
while b"\r\n\r\n" not in raw:
    block = stream.recv(4096)
    assert block
    raw += block
    assert len(raw) <= 128 * 1024
headers, body = raw.split(b"\r\n\r\n", 1)
lines = headers.decode("ascii").split("\r\n")
fields = dict(line.split(":", 1) for line in lines[1:])
fields = {key.lower(): value.strip() for key, value in fields.items()}
length = int(fields["content-length"])
assert 0 <= length <= 96 * 1024
while len(body) < length:
    block = stream.recv(min(4096, length - len(body)))
    assert block
    body += block
assert len(body) == length
assert lines[0] == "POST /v1/systemone HTTP/1.1"
assert fields["authorization"] == "Bearer synthetic-transport-canary"
assert fields["accept-encoding"] == "identity"
assert fields["connection"].lower() == "close"
assert fields["content-type"] == "application/json"
assert fields["user-agent"].startswith("skillranker/")
request = json.loads(body)
assert request["state"] == "synthetic transport probe"

response = json.dumps({
    "model": "synthetic-test-model",
    "answers": {"fit": {"type": "noul", "noul": 0.75}},
    "usage": {"input_tokens": 1, "output_tokens": 1},
}).encode()
# Synthetic regression for the measured 255-option / 0.99-sum failure class.
# These are not captured provider probabilities; raw live bodies are not kept.
if mode in ("choice-255-deficit", "choice-255-valid", "choice-255-rounding"):
    criteria = request["questions"]["rank"]["criteria"]
    assert len(criteria) == 255 and "__none__" in criteria
    total = {"choice-255-deficit": 0.99, "choice-255-valid": 1.0,
             "choice-255-rounding": 0.99995}[mode]
    probabilities = {key: total / 255 for key in criteria}
    response = json.dumps({
        "model": "synthetic-test-model",
        "answers": {"rank": {"type": "choice", "choice": min(criteria),
                             "probabilities": probabilities, "confidence": 0.5}},
        "usage": {"input_tokens": 1, "output_tokens": 1},
    }).encode()

status = "200 OK"
extra = ""
trap = None
if mode == "redirect":
    status = "302 Found"
    trap = socket.socket()
    trap.bind(("127.0.0.1", 0))
    trap.listen(1)
    extra = f"Location: https://localhost:{trap.getsockname()[1]}/credential-trap\r\n"
elif mode == "unavailable":
    status = "503 Unavailable"
    response = b"private-provider-body synthetic-transport-canary"
elif mode == "compression":
    extra = "Content-Encoding: gzip\r\n"
    response = gzip.compress(b"x" * (2 * 1024 * 1024 + 1))
elif mode == "wrong-type":
    extra = "Content-Type: text/plain\r\n"
elif mode == "malformed":
    response = b'{"private-provider-body":'

if mode in ("chunked-success", "chunked-overflow"):
    stream.sendall(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n")
    if mode == "chunked-success":
        middle = len(response) // 2
        pieces = [response[:middle], response[middle:]]
    else:
        pieces = [b"x" * 65536] * 32 + [b"x"]
    try:
        for piece in pieces:
            stream.sendall(f"{len(piece):x}\r\n".encode() + piece + b"\r\n")
        stream.sendall(b"0\r\n\r\n")
    except (ssl.SSLError, ConnectionResetError, BrokenPipeError):
        pass  # The bounded reader can reject the last chunk before its payload.
elif mode == "oversized":
    stream.sendall(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2097153\r\nConnection: close\r\n\r\n")
else:
    stream.sendall((f"HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{extra}Content-Length: {len(response)}\r\nConnection: close\r\n\r\n").encode())
    if mode == "cancel-body":
        emit({"body_pending": True})
    if mode not in ("slow-body", "cancel-body"):
        stream.sendall(response)

# The client must close its connection on success, rejection, deadline and
# cancellation. A TLS close_notify or TCP EOF both establish socket teardown.
try:
    closed = stream.recv(1) == b""
except (ssl.SSLError, ConnectionResetError, BrokenPipeError):
    closed = True
report = {"closed": closed, "requests": 1, "request_bytes": length}
if trap is not None:
    trap.setblocking(False)
    try:
        redirected, _ = trap.accept()
        redirected.close()
        report["redirect_connections"] = 1
    except BlockingIOError:
        report["redirect_connections"] = 0
    trap.close()
emit(report)
stream.close()
