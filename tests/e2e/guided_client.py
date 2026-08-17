#!/usr/bin/env python3
"""End-to-end MCP stdio client for herdr-annotator's guided-review tools.

Speaks the same newline-delimited JSON-RPC 2.0 stdio transport as
mcp_client.py, but drives the non-blocking "guided review" flow instead of
the blocking review_changes tool:

    initialize -> initialized -> tools/call show_changes(...)

show_changes returns immediately (it does not wait for a verdict), so from
there this script reads commands from stdin, one per line, until EOF or a
`quit` line:

    goto <file> <line>   -> tools/call goto {file, line}
    collect [seconds]    -> tools/call collect_review {wait_seconds: seconds or 0}
    quit                 -> stop reading commands and exit

Every tool result is printed to stdout as pretty JSON, preceded by a
"### <tool>" header line, so a driving harness (or a human piping commands
in) can tell the phases apart by scanning for the header.

Usage:
    guided_client.py <working_dir> [note]

Then feed it commands on stdin, e.g.:
    printf 'goto src/mcp.rs 10\ncollect 5\nquit\n' | guided_client.py /path/to/repo

Exits 0 unless show_changes itself returned isError true. Later tool errors
(a goto after the pane is gone, a still-pending collect) are printed but do
not change the exit code — they are normal states in a guided review, not
harness failures. The server's stderr is inherited (passed through to our
stderr) so protocol/debug logs are visible directly.
"""
import json
import subprocess
import sys
import time
from pathlib import Path

BINARY = Path(__file__).resolve().parent.parent.parent / "bin" / "herdr-annotator"
READ_TIMEOUT = 120  # seconds to wait for any single tool call's response line


def send(proc, obj):
    line = json.dumps(obj)
    proc.stdin.write(line + "\n")
    proc.stdin.flush()


def read_response(proc, want_id, timeout):
    """Block reading lines from proc.stdout until one has "id" == want_id."""
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


def call_tool(proc, call_id, name, arguments):
    """Send a tools/call, read its reply, print it under a "### <name>" header.

    Returns the tool result dict (the "result" field of the JSON-RPC reply).
    """
    send(
        proc,
        {
            "jsonrpc": "2.0",
            "id": call_id,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments},
        },
    )
    response = read_response(proc, call_id, timeout=READ_TIMEOUT)
    result = response.get("result", {})
    print(f"### {name}")
    print(json.dumps(result, indent=2))
    sys.stdout.flush()
    return result


def main():
    if len(sys.argv) < 2:
        print("usage: guided_client.py <working_dir> [note]", file=sys.stderr)
        return 2
    working_dir = sys.argv[1]
    note = sys.argv[2] if len(sys.argv) > 2 else "guided e2e test"

    proc = subprocess.Popen(
        [str(BINARY), "mcp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=None,  # inherit: server stderr passes through to our stderr
        text=True,
        bufsize=1,  # line-buffered
    )

    call_id = 1
    try:
        send(
            proc,
            {
                "jsonrpc": "2.0",
                "id": call_id,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "guided-e2e", "version": "0"},
                },
            },
        )
        read_response(proc, call_id, timeout=30)

        send(proc, {"jsonrpc": "2.0", "method": "notifications/initialized"})

        call_id += 1
        show_result = call_tool(
            proc, call_id, "show_changes", {"working_dir": working_dir, "note": note}
        )
        if show_result.get("isError", True):
            return 1

        for raw_line in sys.stdin:
            command = raw_line.strip()
            if not command:
                continue
            parts = command.split()
            verb = parts[0]

            if verb == "quit":
                break

            if verb == "goto":
                if len(parts) != 3:
                    print(f"### goto\nusage: goto <file> <line>, got: {command!r}", file=sys.stderr)
                    continue
                file_arg, line_arg = parts[1], parts[2]
                try:
                    line_no = int(line_arg)
                except ValueError:
                    print(f"### goto\n\"line\" must be an integer, got {line_arg!r}", file=sys.stderr)
                    continue
                call_id += 1
                call_tool(proc, call_id, "goto", {"file": file_arg, "line": line_no})
                continue

            if verb == "collect":
                wait_seconds = 0
                if len(parts) >= 2:
                    try:
                        wait_seconds = int(parts[1])
                    except ValueError:
                        print(
                            f"### collect\n\"seconds\" must be an integer, got {parts[1]!r}",
                            file=sys.stderr,
                        )
                        continue
                call_id += 1
                call_tool(proc, call_id, "collect_review", {"wait_seconds": wait_seconds})
                continue

            print(f"### {verb}\nunknown command: {command!r}", file=sys.stderr)

        return 0
    finally:
        try:
            proc.stdin.close()
        except Exception:
            pass
        try:
            proc.wait(timeout=10)
        except Exception:
            proc.kill()


if __name__ == "__main__":
    sys.exit(main())
