#!/usr/bin/env python3
"""Run the opt-in Claude Code transcript acceptance test on native macOS arm64."""

import argparse
import hashlib
import http.server
import itertools
import json
import platform
import re
import shutil
import subprocess
import tarfile
import tempfile
import threading
import time
import urllib.request
import uuid
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / "tests/fixtures/transcripts/claude-2.1.296"
TEST = "native_lease_turns_and_shared_resume"
DUMMY_KEY = "fixture-dummy-no-credentials"
SYSTEM_PATH = "/usr/bin:/bin:/usr/sbin:/sbin"


def install_binary(directory, binary, capture):
    dest = directory / "claude"
    if binary:
        shutil.copyfile(binary, dest)
    else:
        version = capture["version"]
        package = "claude-code-darwin-arm64"
        url = f"https://registry.npmjs.org/@anthropic-ai/{package}/-/{package}-{version}.tgz"
        print(f"Fetching {capture['binary']}", flush=True)
        with urllib.request.urlopen(url, timeout=90) as package_file:
            with tarfile.open(fileobj=package_file, mode="r|gz") as contents:
                for item in contents:
                    if item.name == "package/claude" and item.isfile():
                        with contents.extractfile(item) as src, dest.open("xb") as out:
                            shutil.copyfileobj(src, out)
                        break
                else:
                    raise RuntimeError("official package omitted the native executable")
    sha = hashlib.sha256()
    with dest.open("rb") as src:
        for chunk in iter(lambda: src.read(1024 * 1024), b""):
            sha.update(chunk)
    if sha.hexdigest() != capture["executableSha256"]:
        raise RuntimeError("native executable differs from pinned capture SHA-256")
    dest.chmod(0o700)


def build_test():
    result = subprocess.run(
        ["cargo", "test", "--locked", "--no-default-features", "--features", "turns",
         "--test", "claude_transcript_live", "--no-run", "--message-format=json-render-diagnostics"],
        cwd=ROOT, check=True, stdout=subprocess.PIPE, text=True,
    )
    for line in result.stdout.splitlines():
        row = json.loads(line)
        if (row.get("reason") == "compiler-artifact" and row["target"]["name"] == "claude_transcript_live"
                and row["profile"]["test"] and row["executable"]):
            return row["executable"]
    raise RuntimeError("cargo omitted the Claude transcript integration test executable")


def event(name, **fields):
    data = json.dumps({"type": name, **fields})
    return f"event: {name}\ndata: {data}\n\n"


class MessagesPeer(http.server.ThreadingHTTPServer):
    def __init__(self, directory):
        super().__init__(("127.0.0.1", 0), Handler)
        self.directory = directory
        self.message_ids = itertools.count(1)
        self.rounds = [(slow, fast, threading.Event())
                       for slow, fast in [("a-one", "b-one"), ("b-two", "a-two")]]


class Handler(http.server.BaseHTTPRequestHandler):
    timeout = 10

    def log_message(self, *_):
        pass

    def respond(self, mime, data):
        self.send_response(200)
        self.send_header("Content-Type", mime)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)
        self.wfile.flush()

    def do_POST(self):
        path = self.path.partition("?")[0]
        if path not in ("/v1/messages", "/v1/messages/count_tokens"):
            self.send_error(404)
            return
        if self.headers.get("x-api-key") != DUMMY_KEY:
            self.send_error(403)
            return
        length = int(self.headers.get("Content-Length", 0))
        if not 0 < length <= 2 * 1024 * 1024:
            self.send_error(413)
            return
        body = json.loads(self.rfile.read(length))
        if path.endswith("/count_tokens"):
            self.respond("application/json", b'{"input_tokens":12}')
            return
        message = {
            "id": f"msg_fixture_{next(self.server.message_ids)}",
            "type": "message", "role": "assistant", "model": body["model"], "content": [],
            "stop_reason": None, "stop_sequence": None,
            "usage": {"input_tokens": 12, "output_tokens": 0},
        }
        if not body.get("stream"):
            # The native CLI can also request a short conversation title.
            message.update(content=[{"type": "text", "text": "Fixture title"}], stop_reason="end_turn")
            self.respond("application/json", json.dumps(message).encode())
            return
        text = json.dumps(body["messages"][-1]["content"])
        matches = re.findall(r"fixture prompt ([a-z-]+)", text)
        name = matches[-1] if matches else None
        reply = f"Fixture response {name}" if name else "Fixture ready"
        for slow, fast, started in self.server.rounds:
            if name == slow:
                started.set()
                # The Rust consumer acknowledges only after its fast receiver and PTY
                # screen have both seen the reply. Socket write completion is earlier.
                marker = self.server.directory / f"received-{fast}"
                deadline = time.monotonic() + 15
                while not marker.is_file():
                    if time.monotonic() >= deadline:
                        self.send_error(504, "other receiver never acknowledged its reply")
                        return
                    time.sleep(0.02)
            if name == fast and not started.wait(timeout=15):
                self.send_error(504, "other terminal never started")
                return
        parts = [
            event("message_start", message=message),
            event("content_block_start", index=0, content_block={"type": "text", "text": ""}),
            event("content_block_delta", index=0, delta={"type": "text_delta", "text": reply}),
            event("content_block_stop", index=0),
            event("message_delta", delta={"stop_reason": "end_turn", "stop_sequence": None},
                  usage={"output_tokens": 4}),
            event("message_stop"),
        ]
        self.respond("text/event-stream", "".join(parts).encode())


def run(directory, binary, harness):
    manifest = json.loads((FIXTURES / "capture.json").read_text())
    install_binary(directory, binary, manifest)
    for name in ["config", "home", "tmp", "project"]:
        (directory / name).mkdir(mode=0o700)
    project = directory / "project"
    config = {
        "hasCompletedOnboarding": True, "lastOnboardingVersion": manifest["version"],
        "theme": "dark", "customApiKeyResponses": {"approved": [DUMMY_KEY[-20:]], "rejected": []},
        "projects": {str(project): {"hasTrustDialogAccepted": True, "hasCompletedProjectOnboarding": True}},
    }
    (directory / "config/.claude.json").write_text(json.dumps(config))
    (directory / "ids").write_text("".join(f"{uuid.uuid4()}\n" for _ in range(3)))
    shutil.copy2(harness, directory / "test")
    with MessagesPeer(directory) as peer:
        port = peer.server_port
        # The test executable and all descendants only write inside scratch or the PTY.
        # Deny real home reads too. The binary and test were copied outside it above.
        home = Path.home().resolve()
        protected = [home, Path("/System/Volumes/Data") / home.relative_to("/")]
        policy = (
            "(version 1)(allow default)(deny network-outbound)"
            f'(allow network-outbound (remote ip "localhost:{port}"))'
            "(deny file-write*)(deny user-preference-write)"
            f'(allow file-write* (subpath {json.dumps(str(directory))}) (literal "/dev/null")'
            ' (literal "/dev/tty") (literal "/dev/ptmx") (regex #"^/dev/ttys[0-9]+$"))'
            '(deny mach-lookup (global-name "com.apple.SecurityServer") (global-name "com.apple.secd")'
            ' (global-name "com.apple.securityd") (global-name "com.apple.securityd.xpc"))'
            "(deny file-read-data " +
            " ".join(f"(subpath {json.dumps(str(p))})" for p in protected) + ")"
        )
        env = {"HOME": str(directory / "home"), "TMPDIR": str(directory / "tmp"),
               "PATH": SYSTEM_PATH, "LANG": "en_US.UTF-8",
               "AGENT_LINK_CLAUDE_FIXTURE": str(directory),
               "AGENT_LINK_CLAUDE_PEER": f"http://127.0.0.1:{port}"}
        thread = threading.Thread(target=peer.serve_forever, daemon=True)
        thread.start()
        try:
            result = subprocess.run(
                ["/usr/bin/sandbox-exec", "-p", policy, str(directory / "test"),
                 TEST, "--exact", "--ignored", "--test-threads=1"],
                cwd=project, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT, text=True, timeout=180,
            )
            print(result.stdout, end="", flush=True)
            result.check_returncode()
            if "1 passed;" not in result.stdout or not all(
                ready.is_set() and (directory / f"received-{fast}").is_file()
                for _, fast, ready in peer.rounds
            ):
                raise RuntimeError("expected the native test and both interleaved exchanges to finish")
        finally:
            peer.shutdown()
            thread.join()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, help="use a local executable matching the pinned SHA-256")
    args = parser.parse_args()
    if (platform.system(), platform.machine()) != ("Darwin", "arm64"):
        parser.error("this recording and process binding require native macOS arm64")
    harness = build_test()
    with tempfile.TemporaryDirectory(dir="/private/tmp", prefix="agent-link-claude-") as directory:
        run(Path(directory), args.binary, harness)


if __name__ == "__main__":
    main()
