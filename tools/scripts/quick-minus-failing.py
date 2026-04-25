#!/usr/bin/env python3
"""Print quick-profile excluded tests minus any listed in failing_tests.txt.

failing_tests.txt may be plain test names, one per line, or raw nextest
output (e.g. `        FAIL [  20.532s] (  1/166) binary::kind  test::name`).
In either case the test name is the last whitespace-separated token on
the line.
"""
import argparse
import re
import sys
from pathlib import Path

TEST_RE = re.compile(r"test\(/\^([^$]+)\$/\)")
SECTION_RE = re.compile(r"^\[profile\.quick\]\s*\n(.*?)(?=^\[|\Z)", re.DOTALL | re.MULTILINE)


def extract_quick_tests(toml_path: Path) -> set[str]:
    m = SECTION_RE.search(toml_path.read_text())
    if not m:
        sys.exit(f"no [profile.quick] section in {toml_path}")
    return set(TEST_RE.findall(m.group(1)))


def load_failing(path: Path) -> set[str]:
    names: set[str] = set()
    for line in path.read_text().splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        names.add(stripped.split()[-1])
    return names


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "--nextest-toml",
        type=Path,
        default=Path("integration-tests/.config/nextest.toml"),
    )
    ap.add_argument("--failing", type=Path, default=Path("failing_tests.txt"))
    ap.add_argument(
        "--counts",
        action="store_true",
        help="print summary counts to stderr alongside the result",
    )
    args = ap.parse_args()

    quick = extract_quick_tests(args.nextest_toml)
    failing = load_failing(args.failing)
    remaining = quick - failing

    if args.counts:
        print(
            f"quick excluded: {len(quick)}  "
            f"failing: {len(failing)}  "
            f"quick ∩ failing: {len(quick & failing)}  "
            f"stable subset (quick - failing): {len(remaining)}",
            file=sys.stderr,
        )

    for t in sorted(remaining):
        print(t)


if __name__ == "__main__":
    main()
