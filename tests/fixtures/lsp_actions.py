import json
import sys
import time
from pathlib import Path

documents = {}
format_mode = Path("format-mode")


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


def position(character):
    return {"line": 0, "character": character}


def span(start=4, end=7):
    return {"start": position(start), "end": position(end)}


def change(replacement):
    return {"range": span(), "newText": replacement}


def diagnostic(version, uri):
    send(method="textDocument/publishDiagnostics", params={
        "uri": uri, "version": version, "diagnostics": [{
            "range": span(), "severity": 2, "message": "Choose a descriptive name.",
            "code": "naming", "data": {"name": "old", "version": version},
        }],
    })


root = None
while message := receive():
    method = message.get("method")
    params = message.get("params") or {}
    uri = params.get("textDocument", {}).get("uri")
    identifier = message.get("id")
    if method == "initialize":
        root = params["rootUri"]
        assert params["capabilities"]["textDocument"]["codeAction"]["resolveSupport"] == {
            "properties": ["edit"],
        }
        capabilities = {
            "positionEncoding": "utf-16", "textDocumentSync": 1, "hoverProvider": True,
            "codeActionProvider": {"resolveProvider": True},
            "renameProvider": {"prepareProvider": True},
        }
        if format_mode.exists():
            assert params["capabilities"]["textDocument"]["formatting"]["dynamicRegistration"]
            capabilities["textDocumentSync"] = {"openClose": True, "change": 1, "save": True}
            if format_mode.read_text() == "static":
                capabilities["documentFormattingProvider"] = {}
        send(id=identifier, result={"capabilities": capabilities})
    elif method == "initialized" and format_mode.exists() and format_mode.read_text() == "dynamic":
        send(id="register-format", method="client/registerCapability", params={"registrations": [{
            "id": "formatter", "method": "textDocument/formatting", "registerOptions": {
                "documentSelector": [{"language": "cpp", "scheme": "file", "pattern": "**/main.cpp"}],
            },
        }]})
    elif method is None and identifier in ("register-format", "unregister-format"):
        assert "result" in message, message
        Path(identifier).touch()
    elif method == "textDocument/didOpen":
        version = params["textDocument"]["version"]
        documents[uri] = (version, params["textDocument"]["text"])
        diagnostic(version, uri)
    elif method == "textDocument/didChange":
        version = params["textDocument"]["version"]
        documents[uri] = (version, params["contentChanges"][0]["text"])
        diagnostic(version, uri)
        if "// unregister" in documents[uri][1] and not Path("unregister-format").exists():
            send(id="unregister-format", method="client/unregisterCapability", params={
                "unregisterations": [{"id": "formatter", "method": "textDocument/formatting"}],
            })
    elif method == "textDocument/didSave" and format_mode.exists():
        Path("format-saved").touch()
    elif method == "textDocument/formatting":
        assert params["options"] == {"tabSize": 2, "insertSpaces": True}
        _, text = documents[uri]
        assert text.startswith("int old = 0;\n// 😀draft"), text
        Path("format-input").write_text(text)
        control = Path("format-result")
        mode = control.read_text() if control.exists() else ""
        edits = [change("formatted"), {
            "range": {"start": {"line": 1, "character": 5}, "end": {"line": 1, "character": 10}},
            "newText": "note",
        }]
        if mode == "delay":
            edits[0]["newText"] = "obsolete"
            Path("format-held").touch()
            deadline = time.monotonic() + 3
            while not Path("format-release").exists() and time.monotonic() < deadline:
                time.sleep(0.005)
        elif mode == "overlap":
            edits.append(change("overlap"))
        elif mode == "invalid":
            edits[1]["range"]["start"]["character"] = 4
        elif mode == "empty":
            edits = None
        elif mode == "same":
            edits = [change("old")]
        elif mode == "error":
            send(id=identifier, error={"code": -32603, "message": "Formatter failed"})
            continue
        send(id=identifier, result=edits)
    elif method == "textDocument/codeAction":
        # Some language servers omit fixes for automatic requests.
        if params["context"]["triggerKind"] != 1:
            send(id=identifier, result=[])
            continue
        version, text = documents[uri]
        if "old" not in text or not (params["range"]["start"]["line"] == 0):
            send(id=identifier, result=[])
            continue
        actions = [{
            "title": "Rename in workspace",
            "kind": "quickfix",
            "command": {"title": "Rename", "command": "clangd.applyRename", "arguments": [{
                "textDocument": {"uri": uri}, "position": position(5), "newName": "renamed",
            }]},
        }, {
            "title": "Fix current draft", "kind": "quickfix", "isPreferred": True,
            "data": {"uri": uri, "version": version},
        }, {
            "title": "Unavailable refactoring", "kind": "refactor",
            "disabled": {"reason": "Fixture reason"},
        }]
        for item in params["context"]["diagnostics"]:
            assert item["data"] == {"name": "old", "version": version}
            assert item["range"] == span()
            if item["code"] == "naming":
                Path("received-diagnostic").touch()
        send(id=identifier, result=actions)
    elif method == "codeAction/resolve":
        data = params["data"]
        version, text = documents[data["uri"]]
        assert data == {"uri": data["uri"], "version": version}
        assert text[4:7] == "old"
        send(id=identifier, result={
            **params, "edit": {"documentChanges": [{
                "textDocument": data, "edits": [change("fixed")],
            }]},
        })
    elif method == "textDocument/hover":
        send(id=identifier, result={"contents": {"kind": "markdown", "value": "**Type** documentation."}, "range": span()})
    elif method == "textDocument/prepareRename":
        send(id=identifier, result={"range": span(), "placeholder": "old"})
    elif method == "textDocument/rename":
        name = params["newName"]
        targets = [uri, root + "/other.cpp"]
        send(id=identifier, result={"documentChanges": [{
            "textDocument": {"uri": target, "version": documents.get(target, (None,))[0]},
            "edits": [change(name)],
        } for target in targets]})
    elif method == "exit":
        break
    elif identifier is not None:
        send(id=identifier, result=None)
