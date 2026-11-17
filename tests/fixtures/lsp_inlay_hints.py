import json
import sys
from pathlib import Path


def receive():
    header = sys.stdin.buffer.readline()
    if not header:
        return None
    length = int(header.split(b":")[1])
    assert sys.stdin.buffer.readline() == b"\r\n"
    return json.loads(sys.stdin.buffer.read(length))


def send(**message):
    body = json.dumps({"jsonrpc": "2.0", **message}).encode()
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode() + body)
    sys.stdout.buffer.flush()


documents = {}
refresh_sent = False
held = None
while message := receive():
    method = message.get("method")
    params = message.get("params") or {}
    identifier = message.get("id")
    uri = params.get("textDocument", {}).get("uri")
    if method == "initialize":
        capabilities = params["capabilities"]
        assert capabilities["textDocument"]["inlayHint"] == {}
        assert capabilities["workspace"]["inlayHint"]["refreshSupport"]
        send(id=identifier, result={"capabilities": {
            "positionEncoding": "utf-16", "textDocumentSync": 1,
            "inlayHintProvider": {"resolveProvider": False},
            "hoverProvider": True, "definitionProvider": True,
        }})
    elif method == "textDocument/didOpen":
        assert uri.startswith("file:") and uri.endswith("/main.cpp")
        documents[uri] = params["textDocument"]["text"]
    elif method == "textDocument/didChange":
        documents[uri] = params["contentChanges"][0]["text"]
    elif method == "textDocument/didClose":
        documents.pop(uri)
    elif method in ("textDocument/hover", "textDocument/definition"):
        source = documents[uri]
        row = params["position"]["line"]
        line = source.splitlines()[row]
        column = len(line[:line.index("anchor")].encode("utf-16-le")) // 2
        assert params["position"]["character"] == column
        span = {"start": {"line": row, "character": column},
                "end": {"line": row, "character": column + len("anchor")}}
        if method == "textDocument/hover":
            send(id=identifier, result={"contents": {"kind": "markdown", "value": line},
                                        "range": span})
        else:
            send(id=identifier, result=[{"targetUri": uri, "targetRange": span,
                                         "targetSelectionRange": span, "originSelectionRange": span}])
    elif method == "textDocument/inlayHint":
        source = documents[uri]
        assert "staged" not in source and "baseline" not in source, source
        start = params["range"]["start"]
        end = params["range"]["end"]
        hints = []
        for row, line in enumerate(source.splitlines()):
            if not start["line"] <= row <= end["line"] or "anchor" not in line:
                continue
            column = len(line[:line.index("anchor")].encode("utf-16-le")) // 2
            if row == end["line"] and column >= end["character"]:
                continue
            label = "long_type " * 36 if "worktree" in line or "waiting" in line else "int"
            hints.append({
                "position": {"line": row, "character": column},
                "label": [{"value": part} for part in (label, ":")],
                "paddingLeft": True,
                "paddingRight": True,
            })
        if "waiting" in source:
            held = (identifier, hints)
            Path("held-inlay").touch()
            continue
        send(id=identifier, result=hints)
        if held is not None:
            send(id=held[0], result=held[1])
            held = None
            Path("late-inlay-sent").touch()
        if not refresh_sent:
            refresh_sent = True
            send(id="refresh", method="workspace/inlayHint/refresh")
    elif identifier == "refresh":
        assert message["result"] is None
        Path("refresh-acknowledged").touch()
    elif method == "exit":
        break
    elif method is not None and identifier is not None:
        send(id=identifier, result=None)
