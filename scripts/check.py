#!/usr/bin/env python3
"""Run the same non-live validation on developer machines and CI."""

import argparse
import os
from pathlib import Path
import subprocess
import sys


def run(*args, env=None):
    print("+ " + " ".join(args), flush=True)
    subprocess.run(args, check=True, env=env)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--skip-tests", action="store_true", help="Only run static checks")
    args = parser.parse_args()
    os.chdir(Path(__file__).resolve().parent.parent)
    run(sys.executable, "scripts/check_docs.py")
    run("cargo", "fmt", "--all", "--", "--check")
    for features in ("", "screen", "observers", "transcript", "turns", "turns,tokio", "questions", "questions,tokio", "approvals", "approvals,codex", "approvals,acp", "approvals,claude", "agent", "tools", "tools,rpc", "tools,codex", "tools,acp", "tools,claude", "tools,tokio", "rpc", "acp", "codex", "claude", "probe", "tokio", "agent,tokio", "rpc,probe,tokio", "codex,tokio", "acp,tokio", "claude,tokio"):
        command = ["cargo", "clippy", "--locked", "--lib", "--no-default-features"]
        if features:
            command += ["--features", features]
        run(*command, "--", "-D", "warnings")
    run("cargo", "clippy", "--locked", "-p", "agent-link-supervisor", "--lib", "--no-default-features", "--", "-D", "warnings")
    run("cargo", "clippy", "--locked", "-p", "agent-link-supervisor", "--lib", "--no-default-features", "--features", "codex", "--", "-D", "warnings")
    run("cargo", "clippy", "--locked", "-p", "agent-link-daemon", "--all-targets", "--no-default-features", "--", "-D", "warnings")

    run("cargo", "clippy", "--locked", "-p", "agent-link-http", "--lib", "--no-default-features", "--", "-D", "warnings")
    run("cargo", "clippy", "--locked", "--workspace", "--all-features", "--all-targets", "--", "-D", "warnings")
    env = os.environ.copy()
    env["RUSTDOCFLAGS"] = (env.get("RUSTDOCFLAGS", "") + " -D warnings").strip()
    run("cargo", "doc", "--locked", "--workspace", "--no-deps", "--all-features", env=env)
    if not args.skip_tests:
        run("cargo", "test", "--locked", "--workspace", "--all-features", "--no-fail-fast", "--", "--test-threads=1")


if __name__ == "__main__":
    main()
