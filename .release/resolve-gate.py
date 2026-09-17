#!/usr/bin/env python3
"""Resolve a gate name from gate-suites.toml to a cargo-nextest --filterset
expression, unioning `extends` cumulatively.

This is the consumer of the gate-suite membership declaration: it turns a gate
name (e.g. `rc-gate`) into the single filterset the runner executes. `extends`
means "run my parent's selection too", realised as a filterset union
`(<parent>) | (<self>)`. Stdlib only (tomllib, Python 3.11+); runs on the CI
runner without extra tooling.
"""

import sys
import tomllib
from pathlib import Path

MANIFEST = Path(__file__).with_name("gate-suites.toml")


def resolve(gate: str, data: dict, seen: tuple = ()) -> str:
    if gate in seen:
        raise SystemExit(f"gate-suites.toml: cyclic `extends` at '{gate}'")
    entry = data.get(gate)
    if entry is None:
        raise SystemExit(f"gate-suites.toml: no gate '{gate}'")
    filterset = entry["filterset"]
    parent = entry.get("extends")
    if parent:
        return f"({resolve(parent, data, seen + (gate,))}) | ({filterset})"
    return filterset


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit("usage: resolve-gate.py <gate-name>")
    data = tomllib.loads(MANIFEST.read_text())
    print(resolve(sys.argv[1], data))


if __name__ == "__main__":
    main()
