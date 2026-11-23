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
assert initialize["method"] == "initialize"
assert initialize["params"]["capabilities"]["textDocument"]["completion"]["contextSupport"]
send(
    id=initialize["id"],
    result={"capabilities": {
        "textDocumentSync": 1,
        "completionProvider": {"triggerCharacters": ["."]},
    }},
)

directory = Path(__file__).parent
source = ""
while (message := receive()) is not None:
    method = message["method"]
    params = message.get("params", {})
    if method == "textDocument/didOpen":
        source = params["textDocument"]["text"]
    elif method == "textDocument/didChange":
        source = params["contentChanges"][0]["text"]
    elif method == "textDocument/completion":
        context = params["context"]
        position = params["position"]
        row = position["line"]
        column = position["character"]
        line = source.split("\n")[row]
        prefix = line.encode("utf-16-le")[: column * 2].decode("utf-16-le")
        with (directory / "requests").open("a") as log:
            log.write(f"{context['triggerKind']} {prefix}\n")
        if prefix.endswith("object."):
            assert column == utf16("  /* 😀 */ object.")
            assert context == {"triggerKind": 2, "triggerCharacter": "."}
            result = {"isIncomplete": True, "items": [{"label": "member"}, {"label": "method"}]}
        elif prefix.endswith("object.me"):
            assert column == utf16("  /* 😀 */ object.me")
            assert context == {"triggerKind": 3}
            start = {"line": row, "character": column - 2}
            end = {"line": row, "character": column}
            insertion = {"start": start, "end": end}
            import_point = {"line": 0, "character": 0}
            invalid = {"line": row, "character": utf16("  /* ") + 1}
            result = {"isIncomplete": False, "items": [
                {"label": "method", "sortText": "90"},
                {"label": "snippet", "filterText": "member", "sortText": "00", "insertTextFormat": 2},
                {"label": "invalid", "filterText": "member", "sortText": "01", "textEdit": {
                    "range": {"start": invalid, "end": end}, "newText": "bad"
                }},
                {
                    "label": "Field via LSP", "filterText": "member", "sortText": "10",
                    "textEdit": {"insert": insertion, "replace": insertion, "newText": "member"},
                    "additionalTextEdits": [{
                        "range": {"start": import_point, "end": import_point},
                        "newText": "#include <widget.h>\n",
                    }],
                },
            ]}
        elif prefix.startswith("let value = sub."):
            assert context == {"triggerKind": 1}
            assert line[len(prefix):] == ".items()"
            start = {"line": row, "character": utf16("let value = sub.")}
            end = {"line": row, "character": column}
            result = [{"label": "fragments", "textEdit": {
                "range": {"start": start, "end": end}, "newText": "fragments"
            }}]
        elif prefix.endswith("objectid"):
            assert context == {"triggerKind": 1}
            result = [{"label": "objectIdentifier"}]
        elif prefix.endswith("object.member"):
            assert context == {"triggerKind": 1}
            (directory / "awaiting-release").touch()
            deadline = time.monotonic() + 5
            while not (directory / "release").exists() and time.monotonic() < deadline:
                time.sleep(0.01)
            result = [{"label": "memberFromStaleReply"}]
        else:
            assert prefix.endswith("object") and context == {"triggerKind": 1}, params
            result = [{"label": "objectValue", "insertText": "objectValue"}]
        send(id=message["id"], result=result)
    elif method == "shutdown":
        send(id=message["id"], result=None)
    elif method == "exit":
        break
    elif "id" in message:
        send(id=message["id"], result=None)
