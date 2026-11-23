import json
import subprocess
import sys


def receive():
    header = sys.stdin.buffer.readline()
    if not header:
        return None
    length = int(header.split(b":")[1])
    assert sys.stdin.buffer.readline() == b"\r\n"
    return json.loads(sys.stdin.buffer.read(length))


def respond(request, result):
    body = json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}).encode()
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode() + body)
    sys.stdout.buffer.flush()


child = None
try:
    while (request := receive()) is not None:
        method = request["method"]
        if method == "initialize":
            respond(request, {"capabilities": {"hoverProvider": True, "textDocumentSync": 1}})
        elif method == "textDocument/hover":
            if child is None:
                child = subprocess.Popen(
                    [
                        sys.executable,
                        "-u",
                        "-c",
                        "import sys; data = bytearray(b'x' * (32 * 1024 * 1024));"
                        " print('ready'); sys.stdin.buffer.read()",
                    ],
                    stdin=subprocess.PIPE,
                    stdout=subprocess.PIPE,
                )
                assert child.stdout.readline() == b"ready\n"
            else:
                child.stdin.close()
                child.wait(timeout=3)
                child = None
            respond(request, None)
        elif method == "shutdown":
            respond(request, None)
        elif method == "exit":
            break
finally:
    if child is not None:
        child.stdin.close()
        child.wait(timeout=3)
