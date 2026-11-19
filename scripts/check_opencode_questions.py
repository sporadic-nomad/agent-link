#!/usr/bin/env python3
"""Exercise OpenCode 1.18.21's question API using a local model and an isolated PTY."""

import argparse
import base64
import hashlib
import json
from pathlib import Path
import platform
import re
import secrets
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import quote
from urllib.request import Request, urlopen

ROOT = Path(__file__).resolve().parent.parent


class Model(BaseHTTPRequestHandler):
    timeout = 5

    def log_message(self, *args):
        pass

    def do_POST(self):
        size = int(self.headers.get("Content-Length", 0))
        if self.path != "/v1/chat/completions" or not 0 < size < 1024 * 1024:
            self.send_error(400)
            return
        request = json.loads(self.rfile.read(size))
        names = {tool["function"]["name"] for tool in request.get("tools", [])}
        results = [m for m in request.get("messages", []) if m["role"] == "tool"]
        answered = bool(results)
        if any("Blue" in json.dumps(m.get("content")) for m in results):
            self.server.received_blue.set()
        if "question" in names and not answered:
            question = {
                "header": "Color",
                "question": "Which color should the sample use?",
                "options": [
                    {"label": "Blue", "description": "Use a cool shade"},
                    {"label": "Amber", "description": "Use a warm shade"},
                ],
                "multiple": False,
            }
            function = {"name": "question", "arguments": json.dumps({"questions": [question]})}
            delta = {
                "tool_calls": [
                    {"index": 0, "id": "call_survey", "type": "function", "function": function}
                ]
            }
            reason = "tool_calls"
        else:
            delta, reason = {"content": "Sample confirmed."}, "stop"
        base = {
            "id": "chatcmpl_survey",
            "object": "chat.completion.chunk",
            "created": 1,
            "model": request.get("model", "probe"),
        }
        first = dict(
            base,
            choices=[{"index": 0, "delta": {"role": "assistant", **delta}, "finish_reason": None}],
        )
        last = dict(
            base,
            choices=[{"index": 0, "delta": {}, "finish_reason": reason}],
            usage={"prompt_tokens": 9, "completion_tokens": 9, "total_tokens": 18},
        )
        body = (
            "".join("data: " + json.dumps(c) + "\n\n" for c in [first, last]) + "data: [DONE]\n\n"
        ).encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def wait_until(poll, seconds=25):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        result = poll()
        if result:
            return result
        time.sleep(0.05)
    raise TimeoutError("isolated OpenCode did not become ready")


def run(binary, test, scratch, peer):
    home = scratch / "fixture-account"
    project = home / "survey"
    project.mkdir(parents=True)
    env = {
        "HOME": str(home),
        "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
        "LANG": "en_US.UTF-8",
        "SHELL": "/bin/sh",
        "OPENCODE_TEST_HOME": str(home),
        "OPENCODE_CLIENT": "cli",
        "OPENCODE_DISABLE_AUTOUPDATE": "true",
        "OPENCODE_DISABLE_MODELS_FETCH": "true",
        "OPENCODE_SERVER_PASSWORD": secrets.token_hex(24),
    }
    for key, name in [
        ("TMPDIR", "tmp"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
    ]:
        (home / name).mkdir()
        env[key] = str(home / name)
    provider = {
        "npm": "@ai-sdk/openai-compatible",
        "name": "Local peer",
        "options": {"baseURL": f"http://127.0.0.1:{peer.server_port}/v1", "apiKey": "dummy"},
        "models": {"probe": {"name": "fixture", "limit": {"context": 32768, "output": 1024}}},
    }
    env["OPENCODE_CONFIG_CONTENT"] = json.dumps(
        {
            "autoupdate": False,
            "snapshot": False,
            "share": "disabled",
            "model": "agent-link/probe",
            "small_model": "agent-link/probe",
            "enabled_providers": ["agent-link"],
            "provider": {"agent-link": provider},
        }
    )
    policy = (
        '(version 1)(allow default)(deny network-outbound)(allow network-outbound (remote ip "localhost:*"))'
        f"(deny file-write* (subpath {json.dumps(str(Path.home()))}))"
    )
    sandbox = ["/usr/bin/sandbox-exec", "-p", policy]
    subprocess.run(["/usr/bin/git", "init", "-q", project], check=True, env=env)
    version = subprocess.check_output(
        [*sandbox, binary, "--version"], cwd=project, env=env, text=True
    ).strip()
    if version != "1.18.21":
        raise RuntimeError(f"expected OpenCode 1.18.21, got {version!r}")
    output = scratch / "server.log"
    with (
        output.open("w") as log,
        subprocess.Popen(
            [*sandbox, binary, "serve", "--hostname", "127.0.0.1", "--port", "0"],
            cwd=project,
            env=env,
            stdout=log,
            stderr=subprocess.STDOUT,
        ) as server,
    ):
        try:

            def listener():
                if server.poll() is not None:
                    raise RuntimeError("OpenCode server exited before binding")
                match = re.search(r"http://127\.0\.0\.1:(\d+)", output.read_text(errors="replace"))
                if match:
                    own = subprocess.run(
                        [
                            "/usr/sbin/lsof",
                            "-nP",
                            "-a",
                            "-p",
                            str(server.pid),
                            "-iTCP:" + match[1],
                            "-sTCP:LISTEN",
                            "-Fn",
                        ],
                        capture_output=True,
                        text=True,
                    )
                    if "n127.0.0.1:" + match[1] in own.stdout:
                        return match[0]
                return None

            endpoint = wait_until(listener)
            credential = base64.b64encode(
                ("opencode:" + env["OPENCODE_SERVER_PASSWORD"]).encode()
            ).decode()

            def api(route, payload=None):
                path = endpoint + route + "?directory=" + quote(str(project), safe="")
                headers = {
                    "Authorization": "Basic " + credential,
                    "Content-Type": "application/json",
                }
                data = None if payload is None else json.dumps(payload).encode()
                with urlopen(Request(path, data=data, headers=headers), timeout=3) as response:
                    raw = response.read(1024 * 1024)
                    return json.loads(raw) if raw else None

            session = api("/session", {"title": "Sample survey"})["id"]
            ready = scratch / "tui.ready"
            env.update(
                AL_QUESTION_BINARY=str(binary),
                AL_QUESTION_DIRECTORY=str(project),
                AL_QUESTION_SESSION=session,
                AL_QUESTION_ADDRESS=endpoint.removeprefix("http://"),
                AL_QUESTION_TUI_READY=str(ready),
            )
            command = [
                *sandbox,
                test,
                "--ignored",
                "--exact",
                "native_question_is_answered_through_its_own_terminal",
                "--test-threads=1",
            ]
            with subprocess.Popen(command, cwd=project, env=env) as tui:
                try:

                    def attached():
                        if tui.poll() is not None:
                            raise RuntimeError("the native test exited before the TUI became ready")
                        return ready.is_file()

                    wait_until(attached)
                    api(
                        "/session/" + session + "/prompt_async",
                        {
                            "model": {"providerID": "agent-link", "modelID": "probe"},
                            "agent": "build",
                            "parts": [{"type": "text", "text": "Survey fixture"}],
                        },
                    )
                    if tui.wait(timeout=40):
                        raise RuntimeError("the native question test failed")
                finally:
                    if tui.poll() is None:
                        tui.kill()
                        tui.wait()
            if any(q["sessionID"] == session for q in api("/question")):
                raise RuntimeError("answered request still pending")
            if not peer.received_blue.wait(timeout=10):
                raise RuntimeError("the local model did not receive the selected answer")
        finally:
            server.terminate()
            try:
                server.wait(timeout=3)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--binary", type=Path, required=True, help="native OpenCode 1.18.21 executable"
    )
    args = parser.parse_args()
    if sys.platform != "darwin" or platform.machine() != "arm64":
        parser.error("the recording uses the native macOS arm64 binary and loopback sandbox")
    binary = args.binary.resolve(strict=True)
    meta = ROOT / "tests/fixtures/opencode-1.18.21-question.meta"
    expected = dict(line.split("=", 1) for line in meta.read_text().splitlines())["binary_sha256"]
    digest = hashlib.sha256()
    with binary.open("rb") as executable:
        for block in iter(lambda: executable.read(1024 * 1024), b""):
            digest.update(block)
    if digest.hexdigest() != expected:
        parser.error("the executable SHA-256 differs from the recorded release")
    build = subprocess.check_output(
        [
            "cargo",
            "test",
            "--offline",
            "--locked",
            "--no-default-features",
            "--features",
            "questions",
            "--test",
            "opencode_questions",
            "--no-run",
            "--message-format=json-render-diagnostics",
        ],
        cwd=ROOT,
        text=True,
    )
    test = next(
        m["executable"]
        for m in map(json.loads, build.splitlines())
        if m.get("target", {}).get("name") == "opencode_questions" and m.get("executable")
    )
    with tempfile.TemporaryDirectory() as tmp, ThreadingHTTPServer(("127.0.0.1", 0), Model) as peer:
        peer.received_blue = threading.Event()
        worker = threading.Thread(target=peer.serve_forever, daemon=True)
        worker.start()
        try:
            run(binary, test, Path(tmp).resolve(), peer)
        finally:
            peer.shutdown()
            worker.join()


if __name__ == "__main__":
    main()
