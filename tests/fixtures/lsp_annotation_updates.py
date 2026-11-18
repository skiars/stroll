import json
from collections import Counter
from pathlib import Path
from queue import Empty, Queue
import sys
from threading import Thread


def read_client():
    while header := sys.stdin.buffer.readline():
        length = int(header.split(b":", 1)[1])
        assert sys.stdin.buffer.readline() == b"\r\n"
        incoming.put(json.loads(sys.stdin.buffer.read(length)))
    incoming.put(None)


def send(message):
    body = json.dumps({"jsonrpc": "2.0", **message}).encode()
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode() + body)
    sys.stdout.buffer.flush()


root = Path(__file__).parent
incoming = Queue()
Thread(target=read_client, daemon=True).start()
names = {
    "textDocument/didOpen": "open",
    "textDocument/didChange": "change",
    "textDocument/documentHighlight": "highlight",
}
counts = Counter()
while True:
    for response in sorted(root.glob("reply-*.json")):
        send(json.loads(response.read_text()))
        response.unlink()
    try:
        message = incoming.get(timeout=0.005)
    except Empty:
        continue
    if message is None:
        break
    method = message["method"]
    if method == "initialize":
        send({"id": message["id"], "result": {"capabilities": {
            "textDocumentSync": 1,
            "documentHighlightProvider": True,
            "codeActionProvider": True,
        }}})
    elif method in names:
        name = names[method]
        counts[name] += 1
        destination = root / f"{name}-{counts[name]}.json"
        staging = destination.with_suffix(".tmp")
        staging.write_text(json.dumps(message))
        staging.replace(destination)
    elif method == "exit":
        break
    elif "id" in message:
        send({"id": message["id"], "result": None})
