import json
import sys
from pathlib import Path


def receive():
    length = int(sys.stdin.buffer.readline().split(b":")[1])
    assert sys.stdin.buffer.readline() == b"\r\n"
    return json.loads(sys.stdin.buffer.read(length))


def send(**message):
    body = json.dumps({"jsonrpc": "2.0", **message}).encode()
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode() + body)
    sys.stdout.buffer.flush()


def expect(method):
    message = receive()
    assert message["method"] == method, message
    return message


def diagnostic(message, end_column=4):
    return {
        "range": {
            "start": {"line": 0, "character": 3},
            "end": {"line": 0, "character": end_column},
        },
        "severity": 1,
        "message": message,
    }


def publish(version, *items):
    send(method="textDocument/publishDiagnostics", params={
        "uri": uri, "version": version, "diagnostics": items,
    })


mode = sys.argv[1]
initialize = expect("initialize")
capabilities = {"textDocumentSync": 1, "hoverProvider": True}
if mode == "pull":
    capabilities["diagnosticProvider"] = {
        "identifier": "test-provider", "interFileDependencies": False, "workspaceDiagnostics": False,
    }
send(id=initialize["id"], result={"capabilities": capabilities})
expect("initialized")
document = expect("textDocument/didOpen")["params"]["textDocument"]
uri = document["uri"]
if mode == "pull":
    first = expect("textDocument/diagnostic")
    assert first["params"]["identifier"] == "test-provider"
    Path("first-pull").touch()
else:
    publish(document["version"], diagnostic("initial"))

current = expect("textDocument/didChange")["params"]["textDocument"]
current_diagnostic = diagnostic("current", end_column=5)
if mode == "pull":
    send(id=first["id"], result={"kind": "full", "items": [diagnostic("outdated")]})
    second = expect("textDocument/diagnostic")
    send(id=second["id"], result={"kind": "full", "items": [current_diagnostic]})
    send(id="refresh", method="workspace/diagnostic/refresh")
    assert receive() == {"jsonrpc": "2.0", "id": "refresh", "result": None}
    third = expect("textDocument/diagnostic")
    send(id=third["id"], result={"kind": "full", "items": []})
else:
    publish(current["version"], current_diagnostic)
    publish(document["version"], diagnostic("outdated"))
    send(id=expect("textDocument/hover")["id"], result=None)
    next_version = expect("textDocument/didChange")["params"]["textDocument"]["version"]
    publish(next_version)

expect("textDocument/didClose")
send(id=expect("shutdown")["id"], result=None)
expect("exit")
