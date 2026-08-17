#!/usr/bin/env python3
"""End-to-end MCP stdio client for herdr-annotator.

Speaks newline-delimited JSON-RPC 2.0 to `herdr-annotator mcp` on stdin/stdout:
initializes, sends the `initialized` notification, then calls the
`review_changes` tool and blocks (no short timeout — a human/agent verdict in
the review pane can take a while) until the tool result arrives.

Usage:
    mcp_client.py <working_dir> [note]

Prints the tool call's `result` field as JSON to stdout. Exits 0 if the
result's `isError` is false, else 1. The server's stderr is inherited
(passed through to our stderr) so protocol/debug logs are visible directly.
"""
import json
import subprocess
import sys
from pathlib import Path

BINARY = Path(__file__).resolve().parent.parent.parent / "bin" / "herdr-annotator"
READ_TIMEOUT = 120  # seconds to wait for the tools/call response line


def send(proc, obj):
    line = json.dumps(obj)
    proc.stdin.write(line + "\n")
    proc.stdin.flush()


def read_response(proc, want_id, timeout):
    """Block reading lines from proc.stdout until one has "id" == want_id.

    Uses proc.stdout.readline() directly rather than select()-based polling:
    each readline() call can legitimately block for a long time waiting on a
    human verdict, so instead we rely on communicate()-free blocking reads —
    fine here because stdin is already fully written and closed by the time
    we wait for the tools/call reply.
    """
    import time

    deadline = time.monotonic() + timeout
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError(f"no response with id={want_id} within {timeout}s")
        line = proc.stdout.readline()
        if line == "":
            raise RuntimeError("server closed stdout before responding")
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        if msg.get("id") == want_id:
            return msg


def main():
    if len(sys.argv) < 2:
        print("usage: mcp_client.py <working_dir> [note]", file=sys.stderr)
        return 2
    working_dir = sys.argv[1]
    note = sys.argv[2] if len(sys.argv) > 2 else "e2e test"

    proc = subprocess.Popen(
        [str(BINARY), "mcp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=None,  # inherit: server stderr passes through to our stderr
        text=True,
        bufsize=1,  # line-buffered
    )

    try:
        send(
            proc,
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "e2e", "version": "0"},
                },
            },
        )
        read_response(proc, 1, timeout=30)

        send(proc, {"jsonrpc": "2.0", "method": "notifications/initialized"})

        send(
            proc,
            {
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "review_changes",
                    "arguments": {"working_dir": working_dir, "note": note},
                },
            },
        )
        response = read_response(proc, 2, timeout=READ_TIMEOUT)
    finally:
        # The server child must never outlive this harness: close its stdin,
        # give it a moment to exit, then kill. A timeout or protocol error
        # above used to propagate past a bare proc.wait(), orphaning the
        # server (observed as inert `herdr-annotator mcp` processes piling
        # up across interrupted test rounds).
        try:
            proc.stdin.close()
        except Exception:
            pass
        try:
            proc.wait(timeout=10)
        except Exception:
            proc.kill()

    result = response.get("result", {})
    print(json.dumps(result, indent=2))

    return 1 if result.get("isError", True) else 0


if __name__ == "__main__":
    sys.exit(main())
