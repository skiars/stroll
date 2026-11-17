import json
import sys


def receive():
    length = int(sys.stdin.buffer.readline().split(b":")[1])
    assert sys.stdin.buffer.readline() == b"\r\n"
    return json.loads(sys.stdin.buffer.read(length))


def send(**message):
    body = json.dumps({"jsonrpc": "2.0", **message}).encode()
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode() + body)
    sys.stdout.buffer.flush()


def create(token):
    send(id="create", method="window/workDoneProgress/create", params={"token": token})
    assert receive() == {"jsonrpc": "2.0", "id": "create", "result": None}


def progress(token, kind, **value):
    send(method="$/progress", params={"token": token, "value": {"kind": kind, **value}})


initialize = receive()
assert initialize["method"] == "initialize"
assert initialize["params"]["capabilities"]["window"]["workDoneProgress"] is True
send(id=initialize["id"], result={"capabilities": {"textDocumentSync": 1, "hoverProvider": True}})
assert receive()["method"] == "initialized"
assert receive()["method"] == "textDocument/didOpen"
create("fetch")
progress("fetch", "begin", title="Fetching")

# A hover request advances each step so the UI can inspect the previous frame.
for step in range(5):
    request = receive()
    assert request["method"] == "textDocument/hover", request
    if step == 0:
        progress("fetch", "report", message="Blocking waiting for file lock on package cache")
    elif step == 1:
        create(42)
        progress(42, "begin", title="Indexing", percentage=20)
    elif step == 2:
        progress(42, "report", message="12/25", percentage=48)
        progress("fetch", "report", message="cargo metadata: finished")
    elif step == 3:
        progress(42, "end")
    else:
        progress("fetch", "end")
    send(id=request["id"], result=None)

while True:
    message = receive()
    if message["method"] == "shutdown":
        send(id=message["id"], result=None)
    elif message["method"] == "exit":
        break
