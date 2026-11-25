import json
import sys
import time
from pathlib import Path

documents = {}
format_mode = Path("format-mode")
command_mode = Path("command-mode")
suppression_mode = Path("suppression-mode")
narrowing_check = "bugprone-narrowing-conversions"


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
    if suppression_mode.exists():
        text = documents[uri][1]
        line = next((i for i, line in enumerate(text.splitlines()) if "return n;" in line), None)
        if line is None:
            return
        location = {"start": {"line": line, "character": 8}, "end": {"line": line, "character": 9}}
        item = {"range": location, "severity": 2, "message": "Narrowing conversion",
                "source": "clang-tidy", "code": narrowing_check}
        diagnostics = [
            {**item, "source": "clang", "code": "-Wconversion"},
            {**item, "code": 42},
            {**item, "code": "bugprone-*"},
        ]
        if f"NOLINT({narrowing_check})" not in text:
            diagnostics.extend([item, item])
        send(method="textDocument/publishDiagnostics", params={
            "uri": uri, "version": version, "diagnostics": diagnostics,
        })
        return
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
        if command_mode.exists():
            capabilities["executeCommandProvider"] = {"commands": ["clangd.applyTweak"]}
        if format_mode.exists():
            assert params["capabilities"]["textDocument"]["formatting"]["dynamicRegistration"]
            capabilities["textDocumentSync"] = {"openClose": True, "change": 1, "save": True}
            if format_mode.read_text() == "static":
                capabilities["documentFormattingProvider"] = {}
        result = {"capabilities": capabilities}
        if suppression_mode.exists():
            result["serverInfo"] = {"name": "clangd"}
        send(id=identifier, result=result)
    elif method == "initialized" and format_mode.exists() and format_mode.read_text() == "dynamic":
        send(id="register-format", method="client/registerCapability", params={"registrations": [{
            "id": "formatter", "method": "textDocument/formatting", "registerOptions": {
                "documentSelector": [{"language": "cpp", "scheme": "file", "pattern": "**/main.cpp"}],
            },
        }]})
    elif method is None and identifier in ("register-format", "unregister-format"):
        assert "result" in message, message
        Path(identifier).touch()
    elif method is None and identifier == "unsolicited-edit":
        assert message["result"]["applied"] is False, message
        Path("unsolicited-edit-denied").touch()
    elif method is None and identifier == "tweak-edit":
        assert message["result"]["applied"] is False, message
        send(id=tweak_request, error={
            "code": -32001,
            "message": "edits were not applied: " + message["result"]["failureReason"],
        })
    elif method == "textDocument/didOpen":
        version = params["textDocument"]["version"]
        documents[uri] = (version, params["textDocument"]["text"])
        diagnostic(version, uri)
        if command_mode.exists():
            send(id="unsolicited-edit", method="workspace/applyEdit", params={
                "edit": {"changes": {uri: [change("unexpected")]}}
            })
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
        if suppression_mode.exists():
            diagnostics = params["context"]["diagnostics"]
            if any(item["code"] == narrowing_check for item in diagnostics):
                send(id=identifier, result=[{
                    "title": "Insert an explicit cast",
                    "kind": "quickfix",
                    "isPreferred": True,
                    "edit": {"changes": {uri: [{"range": diagnostics[0]["range"],
                                               "newText": "static_cast<float>(n)"}]}},
                }])
            else:
                send(id=identifier, result=None)
            continue
        if command_mode.exists():
            edit = {"range": span(32, 35), "newText": "static_cast<float>(old)"}
            send(id=identifier, result=[{
                "title": "Insert an explicit cast",
                "command": "clangd.applyFix",
                "arguments": [{"changes": {uri: [edit]}}],
            }, {
                "title": "Use an explicit conversion",
                "kind": "refactor",
                "command": {"title": "Use an explicit conversion", "command": "clangd.applyTweak",
                            "arguments": [{"file": uri, "selection": span(32, 35), "tweakID": "TestConversion"}]},
            }])
            continue
        if "old" not in text or (params["range"]["start"]["line"] != 0 and "int payload" not in text):
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
    elif method == "workspace/executeCommand":
        assert command_mode.exists()
        assert params["command"] == "clangd.applyTweak", params
        args, = params["arguments"]
        assert args["tweakID"] == "TestConversion" and args["selection"] == span(32, 35)
        assert documents[args["file"]][1][32:35] == "old"
        with Path("executed-tweaks").open("a") as calls:
            calls.write("tweak\n")
        tweak_request = identifier
        send(id="tweak-edit", method="workspace/applyEdit", params={
            "edit": {"changes": {args["file"]: [{"range": span(32, 35), "newText": "float(old)"}]}}
        })
    elif method == "codeAction/resolve":
        data = params["data"]
        version, text = documents[data["uri"]]
        assert data == {"uri": data["uri"], "version": version}
        assert text[4:7] == "old"
        edits = [change("fixed")]
        if "int payload" in text:
            edits = [{
                "range": {"start": position(0), "end": {"line": 2, "character": 0}},
                "newText": "// Header\n" + text.replace("old", "fixed", 1),
            }]
        send(id=identifier, result={
            **params, "edit": {"documentChanges": [{
                "textDocument": data, "edits": edits,
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
