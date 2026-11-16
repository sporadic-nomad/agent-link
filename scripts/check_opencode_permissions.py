"""Explicit live OpenCode v1.18.25 ACP permission check, using a local model peer.

No credentials are copied and no external model service is used. The real CLI runs
the task and read tools in temporary workspaces; every process is killed on exit.
Run only when live validation is intended. --out retains diagnostic logs/results.
"""

import argparse
import json
import os
from pathlib import Path
import selectors
import signal
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MARKER = "AGENT_LINK_EXTERNAL_PERMISSION_PROBE_OK"
SUBAGENT = "AGENT_LINK_SUBAGENT_READ_PROBE"


class ModelPeer(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, external):
        self.external = external
        self.read_requested = threading.Event()
        self.read_observed = False
        self.parent_observed = False
        self.requests = 0
        self.lock = threading.Lock()
        super().__init__(("127.0.0.1", 0), Handler)


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        size = int(self.headers.get("Content-Length", "0"))
        if not 0 < size <= 2**20:
            self.send_error(413)
            return
        request = json.loads(self.rfile.read(size))
        with self.server.lock:
            self.server.requests += 1
            if self.server.requests > 12:
                self.send_error(429)
                return
        messages = request.get("messages", [])
        subagent = any(
            SUBAGENT in json.dumps(m.get("content"))
            for m in messages
            if m["role"] == "user"
        )
        tool_results = [m for m in messages if m["role"] == "tool"]
        observed = MARKER in json.dumps(tool_results)
        with self.server.lock:
            if subagent and observed:
                self.server.read_observed = True
            if not subagent and observed:
                self.server.parent_observed = True
        available = {
            item.get("function", {}).get("name") for item in request.get("tools", [])
        }
        tool = None
        text = None
        if tool_results:
            text = MARKER if observed else "Tool failed to return the external marker."
        elif subagent and "read" in available:
            tool = ("read", {"filePath": str(self.server.external)})
            self.server.read_requested.set()
        elif "task" in available:
            tool = (
                "task",
                {
                    "description": "Probe external read permissions",
                    "subagent_type": "general",
                    "prompt": f"{SUBAGENT}: Read {self.server.external} with the read tool and report its contents.",
                },
            )
        else:
            text = "Permission probe"
        if tool:
            name, arguments = tool
            delta = {
                "role": "assistant",
                "tool_calls": [
                    {
                        "index": 0,
                        "id": "call_" + name,
                        "type": "function",
                        "function": {"name": name, "arguments": json.dumps(arguments)},
                    }
                ],
            }
            finish = "tool_calls"
        else:
            delta = {"role": "assistant", "content": text}
            finish = "stop"
        base = {
            "id": "chatcmpl-permission-probe",
            "object": "chat.completion.chunk",
            "created": 1,
            "model": "probe",
        }
        first = {
            **base,
            "choices": [{"index": 0, "delta": delta, "finish_reason": None}],
        }
        last = {
            **base,
            "choices": [{"index": 0, "delta": {}, "finish_reason": finish}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
        }
        body = (
            "data: "
            + json.dumps(first)
            + "\n\ndata: "
            + json.dumps(last)
            + "\n\ndata: [DONE]\n\n"
        ).encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class Acp:
    def __init__(self, command, env, cwd, stderr):
        self.child = subprocess.Popen(
            command,
            cwd=cwd,
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=stderr,
            start_new_session=True,
            bufsize=0,
        )
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.child.stdout, selectors.EVENT_READ)
        self.buffer = b""
        self.permissions = []

    def send(self, value):
        self.child.stdin.write((json.dumps(value) + "\n").encode())
        self.child.stdin.flush()

    def next(self, deadline):
        while time.monotonic() < deadline:
            if b"\n" in self.buffer:
                line, self.buffer = self.buffer.split(b"\n", 1)
                return json.loads(line)
            if not self.selector.select(max(0, deadline - time.monotonic())):
                return None
            chunk = os.read(self.child.stdout.fileno(), 65536)
            if not chunk:
                raise RuntimeError(f"Unexpected ACP EOF, exit={self.child.poll()}")
            self.buffer += chunk
            if len(self.buffer) > 2**20:
                raise RuntimeError("ACP frame exceeds 1 MiB")
        return None

    def response(self, request_id, timeout=30, read_event=None):
        deadline = time.monotonic() + timeout
        read_deadline = None
        while True:
            if read_event and read_event.is_set() and read_deadline is None:
                read_deadline = time.monotonic() + 3
            message = self.next(
                min(deadline, read_deadline or deadline, time.monotonic() + 0.1)
            )
            if message:
                if message.get("method") == "session/request_permission":
                    self.permissions.append(message["params"])
                    # Native controls must reveal a forwarded approval instead of hiding it.
                    self.send(
                        {
                            "jsonrpc": "2.0",
                            "id": message["id"],
                            "result": {"outcome": {"outcome": "cancelled"}},
                        }
                    )
                if message.get("id") == request_id and "method" not in message:
                    if "error" in message:
                        raise RuntimeError(message["error"])
                    return message["result"]
            if time.monotonic() >= min(deadline, read_deadline or deadline):
                return None

    def close(self):
        self.selector.close()
        if self.child.poll() is None:
            os.killpg(self.child.pid, signal.SIGKILL)
        self.child.wait(timeout=5)
        self.child.stdin.close()
        self.child.stdout.close()


def run(binary, root, label, permission):
    case = root / label
    case.mkdir()
    workspace = case / "workspace"
    workspace.mkdir()
    external = case / "external" / "note.txt"
    external.parent.mkdir()
    external.write_text(MARKER + "\n")
    env = {
        "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
        "HOME": str(case / "home"),
        "TMPDIR": str(case / "tmp"),
        "OPENCODE_DISABLE_AUTOUPDATE": "true",
        "OPENCODE_DISABLE_MODELS_FETCH": "true",
        "OPENCODE_TEST_HOME": str(case / "home"),
    }
    for folder in ["home", "tmp"]:
        (case / folder).mkdir()
    for name, folder in [
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
    ]:
        (case / folder).mkdir()
        env[name] = str(case / folder)
    subprocess.run(["/usr/bin/git", "init", "-q", str(workspace)], env=env, check=True)
    peer = ModelPeer(external)
    worker = threading.Thread(target=peer.serve_forever, daemon=True)
    worker.start()
    env["OPENCODE_CONFIG_CONTENT"] = json.dumps(
        {
            "autoupdate": False,
            "snapshot": False,
            "share": "disabled",
            "model": "agent-link/probe",
            "small_model": "agent-link/probe",
            "enabled_providers": ["agent-link"],
            "provider": {
                "agent-link": {
                    "npm": "@ai-sdk/openai-compatible",
                    "name": "Local permission peer",
                    "options": {
                        "baseURL": f"http://127.0.0.1:{peer.server_port}/v1",
                        "apiKey": "not-a-credential",
                    },
                    "models": {
                        "probe": {
                            "name": "probe",
                            "limit": {"context": 32768, "output": 1024},
                        }
                    },
                }
            },
        }
    )
    if permission is not None:
        env["OPENCODE_PERMISSION"] = permission
    log = open(case / "stderr.log", "wb")
    command = [str(binary), "acp"]
    if sys.platform == "darwin" and Path("/usr/bin/sandbox-exec").exists():
        # Restrict the live child to the local peer, and protect the real user's home.
        real_home = json.dumps(str(Path.home()))
        policy = (
            "(version 1)(allow default)(deny network-outbound)"
            '(allow network-outbound (remote ip "localhost:*"))'
            f"(deny file-write* (subpath {real_home}))"
        )
        command = ["/usr/bin/sandbox-exec", "-p", policy, *command]
    acp = None
    try:
        acp = Acp(command, env, workspace, log)
        acp.send(
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": 1,
                    "clientInfo": {
                        "name": "agent-link-permission-probe",
                        "version": "0.1.0",
                    },
                    "clientCapabilities": {},
                },
            }
        )
        assert acp.response(1), "initialize deadline"
        acp.send(
            {
                "jsonrpc": "2.0",
                "id": 2,
                "method": "session/new",
                "params": {"cwd": str(workspace), "mcpServers": []},
            }
        )
        session = acp.response(2)
        assert session, "session/new deadline"
        acp.send(
            {
                "jsonrpc": "2.0",
                "id": 3,
                "method": "session/prompt",
                "params": {
                    "sessionId": session["sessionId"],
                    "prompt": [
                        {
                            "type": "text",
                            "text": "Run the external permission probe with the task tool.",
                        }
                    ],
                },
            }
        )
        result = acp.response(
            3,
            timeout=45,
            read_event=peer.read_requested if permission is None else None,
        )
        return {
            "case": label,
            "read_requested": peer.read_requested.is_set(),
            "subagent_read_marker": peer.read_observed,
            "parent_received_marker": peer.parent_observed,
            "approval_requests": len(acp.permissions),
            "stop_reason": result.get("stopReason") if result else None,
            "model_requests": peer.requests,
        }
    finally:
        if acp is not None:
            acp.close()
        log.close()
        peer.shutdown()
        peer.server_close()
        worker.join(timeout=3)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--opencode", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="opencode-", dir=args.out))
    version = subprocess.run(
        [str(args.opencode.resolve()), "--version"],
        env={"PATH": "/usr/bin:/bin", "HOME": str(root)},
        cwd=root,
        capture_output=True,
        text=True,
        check=True,
        timeout=30,
    ).stdout.strip()
    if version != "1.18.25":
        raise SystemExit(f"This regression targets OpenCode 1.18.25, found {version!r}")
    print(json.dumps({"version": version, "platform": sys.platform}), flush=True)
    results = []
    for label, permission in [
        ("native", None),
        ("narrow", '{"external_directory":"allow"}'),
        ("unattended", '{"*":"allow"}'),
    ]:
        result = run(args.opencode.resolve(), root, label, permission)
        results.append(result)
        print(json.dumps(result), flush=True)
    (root / "results.json").write_text(json.dumps(results, indent=2) + "\n")
    assert (
        results[0]["read_requested"]
        and not results[0]["subagent_read_marker"]
        and results[0]["stop_reason"] is None
    ), results[0]
    for result in results[1:]:
        assert (
            result["read_requested"]
            and result["subagent_read_marker"]
            and result["parent_received_marker"]
            and result["approval_requests"] == 0
            and result["stop_reason"] == "end_turn"
        ), result
    print(
        "PASS: broad unattended policy retains the narrow external-directory workaround."
    )


if __name__ == "__main__":

    def terminate(_signal, _frame):
        raise SystemExit("Live probe interrupted")

    signal.signal(signal.SIGTERM, terminate)
    main()
