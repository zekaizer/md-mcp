"""Reusable stdio harness for the md-mcp end-to-end suite.

Dependency-free (standard library only): drives the real `md-server --stdio`
binary over its JSON-RPC-on-stdio transport, exactly as an MCP client would.
This exercises the shipped wire protocol — stdout purity, message framing,
process lifecycle — which the in-process rmcp test (ADR-0012) does not.

Two pieces:
  - `MCPClient`     — spawn a server, speak MCP, read structured results.
  - `Runner`        — collect pass/fail checks with section labels and a
                      process-exit summary.
"""

from __future__ import annotations

import json
import os
import subprocess
import threading
from typing import Any, Optional


class MCPError(RuntimeError):
    """The server closed the connection or violated the framing contract."""


class MCPClient:
    """An MCP client over one `md-server --stdio` subprocess.

    The constructor performs the `initialize` handshake, so a returned client
    is ready for `call`. stderr is drained on a background thread and kept for
    diagnostics; stdout carries only JSON-RPC (a stray write there is a bug the
    harness surfaces as an MCPError).
    """

    def __init__(
        self,
        binary: str,
        vault: str,
        extra_env: Optional[dict] = None,
        timeout: float = 30.0,
    ):
        env = dict(os.environ)
        env["MD_VAULT"] = vault
        env.setdefault("RUST_LOG", "warn")
        if extra_env:
            env.update(extra_env)
        self._timeout = timeout
        self.proc = subprocess.Popen(
            [binary, "--stdio"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
            text=True,
            bufsize=1,
        )
        # Popen with PIPE always sets these; assert to document and to narrow.
        assert self.proc.stdin and self.proc.stdout and self.proc.stderr
        self._stdin = self.proc.stdin
        self._stdout = self.proc.stdout
        self._stderr = self.proc.stderr
        self._id = 0
        self.stderr_lines: list[str] = []
        threading.Thread(target=self._drain_stderr, daemon=True).start()
        self._initialize()

    # -- transport ---------------------------------------------------------

    def _drain_stderr(self) -> None:
        for line in self._stderr:
            self.stderr_lines.append(line.rstrip())

    def _send(self, obj: dict) -> None:
        self._stdin.write(json.dumps(obj) + "\n")
        self._stdin.flush()

    def _recv(self) -> dict:
        line = self._stdout.readline()
        if not line:
            raise MCPError(
                "server closed stdout; stderr:\n" + "\n".join(self.stderr_lines)
            )
        return json.loads(line)

    def request(self, method: str, params: Optional[dict] = None) -> dict:
        """Send a request and return the matching response.

        Robust against id-less error responses (a JSON parse error carries no
        id): any response whose id is null or absent is returned to the caller
        rather than looped past, so a protocol-level rejection never hangs the
        client.
        """
        self._id += 1
        req: dict[str, Any] = {"jsonrpc": "2.0", "id": self._id, "method": method}
        if params is not None:
            req["params"] = params
        self._send(req)
        while True:
            msg = self._recv()
            mid = msg.get("id")
            if mid == self._id or mid is None:
                return msg

    def _initialize(self) -> None:
        r = self.request(
            "initialize",
            {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "md-mcp-e2e", "version": "0.0.0"},
            },
        )
        self.server_info = r.get("result", {})
        self._send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    # -- MCP surface -------------------------------------------------------

    def call(self, tool: str, args: Optional[dict] = None) -> dict:
        """Invoke a tool; returns the raw JSON-RPC response."""
        return self.request("tools/call", {"name": tool, "arguments": args or {}})

    def structured(self, tool: str, args: Optional[dict] = None) -> dict:
        """Invoke a tool and return its `structuredContent` (or `{}`)."""
        return self.call(tool, args).get("result", {}).get("structuredContent", {})

    def tools(self) -> list[dict]:
        return self.request("tools/list", {}).get("result", {}).get("tools", [])

    def close(self) -> None:
        try:
            self._stdin.close()
        except Exception:
            pass
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()

    def __enter__(self) -> "MCPClient":
        return self

    def __exit__(self, *_exc) -> None:
        self.close()


class Runner:
    """Collects checks, prints a grouped report, and yields an exit code.

    A suite is a function `run(client, t: Runner)` that calls `t.check(...)`.
    `t.section(...)` labels the following checks in the output.
    """

    def __init__(self, verbose: bool = True):
        self.passed = 0
        self.failed = 0
        self.failures: list[str] = []
        self._section = ""
        self._verbose = verbose

    def section(self, label: str) -> None:
        self._section = label
        if self._verbose:
            print(f"\n— {label}")

    def check(self, name: str, ok: bool, detail: str = "") -> bool:
        tag = f"[{self._section}] {name}" if self._section else name
        if ok:
            self.passed += 1
            if self._verbose:
                print(f"  ok   {name}")
        else:
            self.failed += 1
            self.failures.append(f"{tag} :: {detail}")
            print(f"  FAIL {name} :: {detail}")
        return ok

    def summary(self) -> int:
        total = self.passed + self.failed
        print(f"\n{'=' * 56}")
        print(f"  {self.passed}/{total} passed, {self.failed} failed")
        if self.failures:
            print(f"{'=' * 56}")
            for f in self.failures:
                print(f"  FAIL {f}")
        print(f"{'=' * 56}")
        return 1 if self.failed else 0


def build_fixture(vault: str, path: str, content: str) -> None:
    """Write a fixture note (bytes preserved, no newline translation)."""
    full = os.path.join(vault, path)
    os.makedirs(os.path.dirname(full), exist_ok=True) if os.path.dirname(path) else None
    with open(full, "w", newline="") as f:
        f.write(content)
