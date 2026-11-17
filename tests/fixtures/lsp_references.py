import json
import sys
from pathlib import Path


def receive():
    header = sys.stdin.buffer.readline()
    if not header:
        return {}
    length = int(header.split(b":")[1])
    assert sys.stdin.buffer.readline() == b"\r\n"
    return json.loads(sys.stdin.buffer.read(length))


def reply(request, result):
    response = {"jsonrpc": "2.0", "id": request["id"], "result": result}
    body = json.dumps(response, ensure_ascii=False).encode()
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode() + body)
    sys.stdout.buffer.flush()


def location(uri, line, start, end):
    return {
        "uri": uri,
        "range": {
            "start": {"line": line, "character": start},
            "end": {"line": line, "character": end},
        },
    }


root = Path.cwd()
main = (root / "main.cpp").as_uri()
target = (root / "使用 #.cpp").as_uri()
declaration = (root / "symbol.cpp").as_uri()
documents = {}

while message := receive():
    method = message["method"]
    params = message.get("params", {}) or {}
    if method == "initialize":
        assert "references" in params["capabilities"]["textDocument"]
        reply(message, {"capabilities": {"referencesProvider": {}, "textDocumentSync": 1}})
    elif method == "textDocument/didOpen":
        documents[params["textDocument"]["uri"]] = params["textDocument"]["text"]
    elif method == "textDocument/didChange":
        documents[params["textDocument"]["uri"]] = params["contentChanges"][0]["text"]
    elif method == "textDocument/references":
        assert params["textDocument"]["uri"] == main
        assert documents[main] == "😀 word();\n"
        assert documents[target] == "// draft\n😀 word();\n"
        if params["position"] == {"line": 0, "character": 0}:
            reply(message, None)
            continue
        assert params["position"]["line"] == 0 and 3 <= params["position"]["character"] < 7
        include = params["context"]["includeDeclaration"]
        assert isinstance(include, bool)
        usage = location(target, 1, 3, 7)
        results = [usage, usage, location("file:///outside.cpp", 0, 0, 4)]
        if include:
            results.append(location(declaration, 0, 4, 8))
        reply(message, results)
    elif method == "shutdown":
        reply(message, None)
    elif method == "exit":
        break
