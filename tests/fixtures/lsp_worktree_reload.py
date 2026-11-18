import json
import os
from pathlib import Path
import sys


def receive():
    header = sys.stdin.buffer.readline()
    if not header:
        return None
    length = int(header.split(b":")[1])
    assert sys.stdin.buffer.readline() == b"\r\n"
    return json.loads(sys.stdin.buffer.read(length))


def respond(message, result):
    body = json.dumps({"jsonrpc": "2.0", "id": message["id"], "result": result}).encode()
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode() + body)
    sys.stdout.buffer.flush()


def record(event, **data):
    with (Path.cwd() / marker).open("a", encoding="utf-8") as log:
        log.write(json.dumps({"event": event, "pid": os.getpid(), "profile": profile, **data}) + "\n")


marker, profile = sys.argv[1:]
record("start")
while message := receive():
    method = message["method"]
    if method == "initialize":
        respond(message, {"capabilities": {"textDocumentSync": 1}})
    elif method == "textDocument/didOpen":
        record("open", **message["params"]["textDocument"])
    elif method == "exit":
        record("exit")
        break
    elif "id" in message:
        respond(message, None)
