import json
from pathlib import Path
import sys
import time


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


def utf16(text):
    return len(text.encode("utf-16-le")) // 2


initialize = receive()
capabilities = initialize["params"]["capabilities"]["textDocument"]
assert "documentHighlight" in capabilities
assert capabilities["signatureHelp"]["signatureInformation"]["parameterInformation"]["labelOffsetSupport"]
send(id=initialize["id"], result={"capabilities": {
    "textDocumentSync": 1,
    "documentHighlightProvider": True,
    "signatureHelpProvider": {"triggerCharacters": ["(", ","]},
    "completionProvider": {},
    "codeActionProvider": True,
}})

directory = Path(__file__).parent
source = ""
while (message := receive()) is not None:
    method = message["method"]
    params = message.get("params", {})
    if method == "textDocument/didOpen":
        source = params["textDocument"]["text"]
    elif method == "textDocument/didChange":
        source = params["contentChanges"][0]["text"]
    elif method == "textDocument/documentHighlight":
        pos = params["position"]
        line = source.split("\n")[pos["line"]]
        word_prefix = "/* 😀 */ "
        if line.startswith(word_prefix) and pos["character"] == utf16(word_prefix) + 1:
            (directory / "highlight-requested").touch()
            start = utf16(word_prefix)
            result = [
                {"range": {"start": {"line": 0, "character": start},
                           "end": {"line": 0, "character": start + 5}}, "kind": 2},
                {"range": {"start": {"line": 1, "character": 0},
                           "end": {"line": 1, "character": 5}}, "kind": 3},
            ]
        else:
            result = None
        send(id=message["id"], result=result)
    elif method == "textDocument/signatureHelp":
        pos = params["position"]
        line = source.split("\n")[pos["line"]]
        prefix = line.encode("utf-16-le")[:pos["character"] * 2].decode("utf-16-le")
        context = params["context"]
        result = None
        if prefix == "invoke(":
            if context.get("triggerCharacter") == "(":
                (directory / "triggered").touch()
            if context["triggerKind"] == 1:
                (directory / "invoked").touch()
            result = {"signatures": [
                {"label": "invoke(int first)"},
                {"label": "invoke(int first, int second)", "parameters": [
                    {"label": [7, 16]}, {"label": [18, 28]},
                ]},
            ], "activeSignature": 1, "activeParameter": 0}
        elif prefix == "invoke(1,":
            assert context["triggerCharacter"] == ","
            (directory / "delayed").touch()
            deadline = time.monotonic() + 5
            while not (directory / "release").exists() and time.monotonic() < deadline:
                time.sleep(0.01)
            result = {"signatures": [{"label": "stale(second)"}], "activeParameter": 1}
        send(id=message["id"], result=result)
        if prefix == "invoke(1,":
            (directory / "replied").touch()
    elif method == "textDocument/completion":
        send(id=message["id"], result=[{"label": "candidate"}])
    elif method == "textDocument/codeAction":
        send(id=message["id"], result=[{"title": "Fix", "kind": "quickfix"}])
    elif method == "shutdown":
        send(id=message["id"], result=None)
    elif method == "exit":
        break
    elif "id" in message:
        send(id=message["id"], result=None)
