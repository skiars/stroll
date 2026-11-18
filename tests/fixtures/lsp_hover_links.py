import json
import sys
from pathlib import Path


def send(identifier, result):
    body = json.dumps({"jsonrpc": "2.0", "id": identifier, "result": result}).encode()
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode() + body)
    sys.stdout.buffer.flush()


while header := sys.stdin.buffer.readline():
    length = int(header.split(b":", 1)[1])
    assert sys.stdin.buffer.readline() == b"\r\n"
    message = json.loads(sys.stdin.buffer.read(length))
    method = message["method"]
    if method == "initialize":
        send(message["id"], {"capabilities": {"hoverProvider": True, "textDocumentSync": 1}})
    elif method == "textDocument/hover":
        markdown = Path(__file__).with_suffix(".md").read_text(encoding="utf-8")
        send(message["id"], {"contents": {"kind": "markdown", "value": markdown}})
    elif method == "exit":
        break
    elif "id" in message:
        send(message["id"], None)
