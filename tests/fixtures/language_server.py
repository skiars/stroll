import json
import sys
from pathlib import Path


def receive():
    length = int(sys.stdin.buffer.readline().split(b":")[1])
    assert sys.stdin.buffer.readline() == b"\r\n"
    return json.loads(sys.stdin.buffer.read(length))


def send(**message):
    body = json.dumps({"jsonrpc": "2.0", **message}, ensure_ascii=False).encode()
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode() + body)
    sys.stdout.buffer.flush()


def expect(method):
    message = receive()
    assert message["method"] == method, message
    return message


initialize = expect("initialize")
assert initialize["params"]["capabilities"]["general"]["positionEncodings"] == ["utf-16"]
send(
    id=initialize["id"],
    result={
        "capabilities": {
            "hoverProvider": True,
            "definitionProvider": True,
            "textDocumentSync": {"openClose": True, "change": 2, "save": {"includeText": True}},
            "semanticTokensProvider": {
                "legend": {"tokenTypes": ["function"], "tokenModifiers": []},
                "range": True,
            },
        }
    },
)
expect("initialized")
definition = expect("textDocument/didOpen")["params"]["textDocument"]
document = expect("textDocument/didOpen")["params"]["textDocument"]
uri = document["uri"]
assert uri.endswith("%E7%94%B2%20%23.rs"), uri
assert document["text"] == "😀 old\n"
send(id="refresh", method="workspace/semanticTokens/refresh")
assert receive() == {"jsonrpc": "2.0", "id": "refresh", "result": None}

change = expect("textDocument/didChange")["params"]
assert change["textDocument"] == {"uri": uri, "version": 2}
assert change["contentChanges"] == [
    {
        "range": {"start": {"line": 0, "character": 0}, "end": {"line": 1, "character": 0}},
        "text": "😀 调用()\n",
    }
]
hover = expect("textDocument/hover")
assert hover["params"]["position"] == {"line": 0, "character": 3}
send(
    id=hover["id"],
    result={
        "contents": {"kind": "markdown", "value": "**跨文件文档**"},
        "range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 5}},
    },
)

tokens = expect("textDocument/semanticTokens/range")
assert tokens["params"]["range"]["end"] == {"line": 1, "character": 0}
send(id=tokens["id"], result={"data": [0, 3, 2, 0, 0]})
jump = expect("textDocument/definition")
send(
    id=jump["id"],
    result={
        "uri": definition["uri"],
        "range": {"start": {"line": 0, "character": 7}, "end": {"line": 0, "character": 17}},
    },
)

save = expect("textDocument/didSave")["params"]
assert save == {"textDocument": {"uri": uri}, "text": "😀 调用()\n"}
assert expect("textDocument/didClose")["params"]["textDocument"]["uri"] == uri
assert expect("textDocument/didClose")["params"]["textDocument"]["uri"] == definition["uri"]
send(id=expect("shutdown")["id"], result=None)
expect("exit")
Path("server-exited").touch()
