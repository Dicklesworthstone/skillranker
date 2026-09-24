"""Real loopback TLS Jev provider for rank pipeline tests; synthetic data only.

Usage: provider_server.py SCENARIO[+write-on-wide|+write-on-rerank] [PATH [TEXT]]

Serves sequential POST /v1/systemone requests over TLS and answers every
question in each request from the request itself, so option IDs are never
guessed. One JSON line is emitted per request. A plaintext connection that
sends DONE ends the run with a final report line.

Scenarios:
  useful          high wide gate; rerank favors its first skill with high fits
  low-need        low wide gate, so no rerank should follow
  none            rerank puts most mass on __none__
  low-fit         rerank favors a skill but every fit is low
  write-on-wide   like useful; before answering wide, write TEXT to PATH
  touch-on-rerank like useful; before answering rerank, append to PATH
  late-rerank     like useful; the rerank answer is delayed by TEXT seconds
  retry-wide      like useful, but the first wide attempt is a 503 with
                  Retry-After: 0
  always-503      every attempt is a 503 with Retry-After: 0
  unauthorized    every attempt is a 401
  slow-wide       like useful; the wide answer is delayed by TEXT seconds
  drift           like useful, but every choice distribution sums to 0.92,
                  as live totals drift from one
"""

import json
import pathlib
import socket
import ssl
import sys
import time

root = pathlib.Path(__file__).parent
scenario, _, mutation = sys.argv[1].partition("+")
target = pathlib.Path(sys.argv[2]) if len(sys.argv) > 2 else None
text = sys.argv[3] if len(sys.argv) > 3 else ""
USAGE = {"wide": (100, 25), "rerank": (120, 30)}


def emit(value):
    print(json.dumps(value), flush=True)


def distribution(options, favored):
    if len(options) == 1:
        return {options[0]: 1.0}
    rest = [option for option in options if option != favored]
    share = 0.4 / len(rest)
    probabilities = {option: share for option in rest}
    probabilities[favored] = 1.0 - share * len(rest)
    return probabilities


def answer(key, question, stage):
    if question["type"] == "choice":
        options = list(question["criteria"])
        skills = [option for option in options if option != "__none__"]
        favored = skills[0] if skills else options[0]
        if stage == "rerank" and key == "rerank" and scenario == "none":
            favored = "__none__"
        probabilities = distribution(options, favored)
        if scenario == "drift":
            probabilities = {option: p * 0.92 for option, p in probabilities.items()}
        return {"type": "choice", "choice": favored, "probabilities": probabilities,
                "confidence": 0.8}
    value = 0.5
    if key.startswith("gate::"):
        need = scenario != "low-need"
        value = {"gate::specialized_method": 0.85 if need else 0.1,
                 "gate::material_help": 0.9 if need else 0.1,
                 "gate::context_suffices": 0.1 if need else 0.9}.get(key, 0.5)
    elif key.startswith("fits::"):
        value = 0.1 if scenario == "low-fit" else 0.8
    return {"type": "noul", "noul": value}


def read_request(stream):
    raw = b""
    while b"\r\n\r\n" not in raw:
        block = stream.recv(4096)
        assert block
        raw += block
        assert len(raw) <= 128 * 1024
    headers, body = raw.split(b"\r\n\r\n", 1)
    lines = headers.decode("ascii").split("\r\n")
    fields = {key.lower(): value.strip()
              for key, value in (line.split(":", 1) for line in lines[1:])}
    length = int(fields["content-length"])
    assert 0 <= length <= 96 * 1024
    while len(body) < length:
        block = stream.recv(min(4096, length - len(body)))
        assert block
        body += block
    assert lines[0] == "POST /v1/systemone HTTP/1.1"
    assert fields["content-type"] == "application/json"
    return fields, json.loads(body), length, body.decode("utf-8")


context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(root / "server.pem", root / "server.key")
context.set_alpn_protocols(["http/1.1"])
listener = socket.socket()
listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
listener.bind(("127.0.0.1", 0))
listener.listen(4)
listener.settimeout(30)
emit({"port": listener.getsockname()[1]})

stages = []
while True:
    connection, _ = listener.accept()
    connection.settimeout(10)
    if connection.recv(4, socket.MSG_PEEK) == b"DONE":
        connection.close()
        break
    try:
        stream = context.wrap_socket(connection, server_side=True)
    except (ssl.SSLError, ConnectionResetError):
        # A client that does not trust the fixture CA ends the handshake.
        emit({"handshake_rejected": True})
        connection.close()
        continue
    fields, request, length, body_text = read_request(stream)
    questions = request["questions"]
    stage = "wide" if "which" in questions else "rerank" if "rerank" in questions else "other"
    stages.append(stage)
    if (scenario == "write-on-wide" and stage == "wide") or mutation == f"write-on-{stage}":
        target.write_text(text)
    if scenario == "touch-on-rerank" and stage == "rerank":
        with target.open("a") as handle:
            handle.write("\nChanged while the provider answered.\n")
    if scenario == "late-rerank" and stage == "rerank":
        time.sleep(float(text))
    if scenario == "slow-wide" and stage == "wide":
        time.sleep(float(text))
    status = "200 OK"
    if scenario == "always-503" or (
            scenario == "retry-wide" and stages.count("wide") == 1 and stage == "wide"):
        status = "503 Service Unavailable"
    elif scenario == "unauthorized":
        status = "401 Unauthorized"
    inputs, outputs = USAGE.get(stage, (1, 1))
    body = json.dumps({
        "model": "jev-test",
        "answers": {key: answer(key, question, stage) for key, question in questions.items()},
        "usage": {"input_tokens": inputs, "output_tokens": outputs},
    }).encode()
    extra = ""
    if status != "200 OK":
        body = b'{"error": "synthetic provider failure"}'
        extra = "Retry-After: 0\r\n" if status.startswith("503") else ""
    emit({"stage": stage, "status": int(status.split()[0]), "request_bytes": length,
          "options": len(questions.get("which", questions.get("rerank", {"criteria": {}}))[
              "criteria"]),
          "authorization": "authorization" in fields, "body": body_text})
    try:
        stream.sendall((f"HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{extra}"
                        f"Content-Length: {len(body)}\r\nConnection: close\r\n\r\n").encode()
                       + body)
        stream.settimeout(0.05)
        stream.recv(1)
    except (ssl.SSLError, ConnectionResetError, BrokenPipeError, socket.timeout):
        pass  # A client past its deadline may already have closed the socket.
    stream.close()

emit({"done": True, "requests": len(stages), "stages": stages})
listener.close()
