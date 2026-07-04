#!/usr/bin/env python3
"""End-to-end test runner for md-mcp over stdio.

Builds the release `md-server` binary (unless `--no-build`), then drives it
through the functional and hardening suites in isolated temp vaults. Prints a
combined summary and exits non-zero if any check fails — suitable as a
pre-commit / pre-merge / pre-push gate (see `make check`).

Usage:
    python3 tests/e2e/run.py [--no-build] [--only functional|hardening]
                             [--binary PATH] [-q]

No third-party dependencies; Python 3.9+ and a built server are all it needs.
"""

from __future__ import annotations

import argparse
import os
import subprocess
import sys
import tempfile

# Make `harness` and `suites` importable regardless of the caller's cwd.
HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

from harness import MCPClient, Runner  # noqa: E402
from suites import fuzz, functional, hardening  # noqa: E402

REPO_ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
DEFAULT_BINARY = os.path.join(REPO_ROOT, "target", "release", "md-server")


def build_binary(quiet: bool) -> None:
    print("• building release md-server …")
    r = subprocess.run(
        ["cargo", "build", "--release", "-p", "md-server"],
        cwd=REPO_ROOT,
        capture_output=quiet,
        text=True,
    )
    if r.returncode != 0:
        if quiet and r.stderr:
            sys.stderr.write(r.stderr)
        sys.exit("build failed; fix compilation before running e2e")


def run_functional(binary: str, t: Runner) -> None:
    print("\n### functional")
    with tempfile.TemporaryDirectory(prefix="mdmcp-e2e-func-") as scratch:
        vault = os.path.join(scratch, "vault")
        os.makedirs(vault)
        functional.build_fixtures(vault)
        with MCPClient(binary, vault) as c:
            functional.run(c, t, vault)


def run_hardening(binary: str, t: Runner) -> None:
    print("\n### hardening")
    with tempfile.TemporaryDirectory(prefix="mdmcp-e2e-hard-") as scratch:
        hardening.run(binary, t, scratch)


def run_fuzz(binary: str, t: Runner, iters: int, seed: int) -> None:
    print("\n### fuzz")
    with tempfile.TemporaryDirectory(prefix="mdmcp-e2e-fuzz-") as scratch:
        fuzz.run(binary, t, scratch, iters=iters, seed=seed)


def main() -> int:
    ap = argparse.ArgumentParser(description="md-mcp stdio end-to-end tests")
    ap.add_argument("--no-build", action="store_true", help="use the existing binary")
    ap.add_argument("--only", choices=["functional", "hardening", "fuzz"], help="run one suite")
    ap.add_argument("--binary", default=DEFAULT_BINARY, help="path to md-server")
    ap.add_argument("--fuzz-iters", type=int, default=250,
                    help="fuzz rounds per tool (default 250)")
    ap.add_argument("--fuzz-seed", type=int, default=1337,
                    help="fuzz RNG seed for reproducible campaigns (default 1337)")
    ap.add_argument("-q", "--quiet", action="store_true", help="quieter build + only failures")
    args = ap.parse_args()

    if not args.no_build:
        build_binary(args.quiet)
    if not os.path.exists(args.binary):
        sys.exit(f"binary not found: {args.binary} (drop --no-build to build it)")

    t = Runner(verbose=not args.quiet)
    # Default gate = functional + hardening (deterministic). Fuzz is opt-in
    # (`--only fuzz`): a randomized campaign does not belong in the pre-push gate.
    if args.only == "fuzz":
        run_fuzz(args.binary, t, args.fuzz_iters, args.fuzz_seed)
        return t.summary()
    if args.only != "hardening":
        run_functional(args.binary, t)
    if args.only != "functional":
        run_hardening(args.binary, t)
    return t.summary()


if __name__ == "__main__":
    raise SystemExit(main())
