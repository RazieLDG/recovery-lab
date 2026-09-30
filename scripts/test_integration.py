#!/usr/bin/env python3
"""Repeatable standard-library test runner; never installs or downloads tools."""
import argparse
import os
from pathlib import Path
import sys
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
from tests.support import recovery_binary, toxiproxy_binary  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--suite", choices=("all", "fixture", "mock", "real"), default="all")
    parser.add_argument("--binary", help="path to an already built recovery-lab executable")
    parser.add_argument("--toxiproxy", help="path to an already installed toxiproxy-server")
    parser.add_argument("--require-real", action="store_true", help="fail instead of skip if real Toxiproxy is unavailable")
    args = parser.parse_args()
    if args.binary:
        os.environ["RECOVERY_LAB_BIN"] = args.binary
    if args.toxiproxy:
        os.environ["TOXIPROXY_BIN"] = args.toxiproxy
    try:
        if args.suite != "fixture" and recovery_binary() is None:
            parser.error("CLI binary unavailable: run cargo build or pass --binary /path/to/recovery-lab")
        if args.require_real and args.suite not in ("all", "real"):
            parser.error("--require-real requires --suite all or --suite real")
        if args.require_real and toxiproxy_binary() is None:
            parser.error("real Toxiproxy required: pass --toxiproxy or set TOXIPROXY_BIN")
    except RuntimeError as exc:
        parser.error(str(exc))
    names = {"fixture": "tests.test_fixture", "mock": "tests.test_mock_contract", "real": "tests.test_real_toxiproxy"}
    modules = list(names.values()) if args.suite == "all" else [names[args.suite]]
    suite = unittest.defaultTestLoader.loadTestsFromNames(modules)
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    return 0 if result.wasSuccessful() else 1


if __name__ == "__main__":
    raise SystemExit(main())
