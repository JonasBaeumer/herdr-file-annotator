#!/usr/bin/env python3
"""Regression check: guided_client.read_response's deadline must fire even
when the child goes fully silent, not just when it answers with the wrong id.

A bare readline() blocks forever on true silence, so the per-iteration
deadline check inside read_response's loop never gets a chance to run —
READ_TIMEOUT would be decorative for a genuinely hung MCP server (the actual
failure mode this guards against). Not part of `cargo test` or CI; this
harness has no automated runner (same as guided_client.py / mcp_client.py),
so run it directly:

    python3 tests/e2e/test_read_timeout.py
"""
import subprocess
import sys
import time

from guided_client import read_response


def main():
    # A child that consumes one line (so our write doesn't fail) and then
    # goes completely silent — never writes a byte back, unlike a child
    # that merely answers with the wrong id.
    proc = subprocess.Popen(
        [sys.executable, "-c", "import sys, time; sys.stdin.readline(); time.sleep(120)"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        text=True,
        bufsize=1,
    )
    try:
        proc.stdin.write("go\n")
        proc.stdin.flush()

        start = time.monotonic()
        try:
            read_response(proc, want_id=1, timeout=1)
        except TimeoutError:
            elapsed = time.monotonic() - start
        else:
            print("FAIL: read_response returned instead of timing out", file=sys.stderr)
            return 1

        # Generous slack over the 1s deadline: proves the timeout fired near
        # its bound (the fix), not merely "eventually" via some unrelated path.
        if elapsed > 5:
            print(f"FAIL: timeout took {elapsed:.1f}s to fire, wanted ~1s", file=sys.stderr)
            return 1

        print(f"OK: TimeoutError raised after {elapsed:.2f}s despite a fully silent child")
        return 0
    finally:
        proc.kill()
        try:
            proc.wait(timeout=5)
        except Exception:
            pass


if __name__ == "__main__":
    sys.exit(main())
