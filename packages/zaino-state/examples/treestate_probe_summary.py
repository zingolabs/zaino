#!/usr/bin/env python3
"""Summarize treestate_probe CSV output so the per-height cost knee is obvious.

Reads the CSV emitted by the `treestate_probe` example
(`height,rpc_seconds,parse_seconds,sapling_size,orchard_size`) and prints, per
row, an ASCII bar scaled to the slowest `rpc_seconds`, plus split statistics
above/below Orchard (NU5) activation. The shape of the bars shows whether — and
where — `z_gettreestate` latency climbs.

Usage:
    treestate_probe ... > sweep.csv
    ./treestate_probe_summary.py sweep.csv
    # or pipe:
    treestate_probe ... | ./treestate_probe_summary.py
"""

import sys

NU5_HEIGHT = 1_687_104  # mainnet Orchard activation
BAR_WIDTH = 48
EIGHTHS = " ▏▎▍▌▋▊▉█"


def median(values):
    if not values:
        return None
    ordered = sorted(values)
    mid = len(ordered) // 2
    if len(ordered) % 2:
        return ordered[mid]
    return (ordered[mid - 1] + ordered[mid]) / 2.0


def bar(value, scale):
    """Unicode block bar for `value` against full-scale `scale`."""
    if scale <= 0:
        return ""
    eighths = round((value / scale) * BAR_WIDTH * 8)
    full, rem = divmod(eighths, 8)
    return ("█" * full) + (EIGHTHS[rem] if rem else "")


def main():
    source = open(sys.argv[1]) if len(sys.argv) > 1 else sys.stdin
    rows = []
    errors = 0
    with source:
        for line in source:
            line = line.strip()
            if not line or line.startswith("height,"):
                continue
            parts = line.split(",")
            try:
                height = int(parts[0])
                rpc = float(parts[1]) if parts[1] else None
            except (ValueError, IndexError):
                continue
            parse = float(parts[2]) if len(parts) > 2 and parts[2] else None
            orchard = int(parts[4]) if len(parts) > 4 and parts[4] else None
            if parse is None:  # blank parse column == RPC errored/timed out
                errors += 1
            rows.append((height, rpc, parse, orchard))

    if not rows:
        print("no data rows found", file=sys.stderr)
        return 1

    rpc_values = [r[1] for r in rows if r[1] is not None]
    scale = max(rpc_values) if rpc_values else 0.0

    print(f"{'height':>10} {'rpc_s':>9} {'parse_s':>9} {'orchard':>11}  rpc_seconds")
    print("-" * 78)
    for height, rpc, parse, orchard in rows:
        marker = "  <- NU5" if height == NU5_HEIGHT else ""
        rpc_str = f"{rpc:9.4f}" if rpc is not None else f"{'—':>9}"
        parse_str = f"{parse:9.4f}" if parse is not None else f"{'ERR':>9}"
        orc_str = f"{orchard:11d}" if orchard is not None else f"{'—':>11}"
        bar_str = bar(rpc, scale) if rpc is not None else ""
        print(f"{height:10d} {rpc_str} {parse_str} {orc_str}  {bar_str}{marker}")

    below = [r[1] for r in rows if r[0] < NU5_HEIGHT and r[1] is not None]
    above = [r[1] for r in rows if r[0] >= NU5_HEIGHT and r[1] is not None]
    parse_all = [r[2] for r in rows if r[2] is not None]

    print("-" * 78)
    print(f"rows: {len(rows)}   errors/timeouts: {errors}")
    bmed, amed = median(below), median(above)
    if below:
        print(f"below NU5  (n={len(below):>3}): rpc median {bmed:.4f}s  max {max(below):.4f}s")
    if above:
        print(f"at/above   (n={len(above):>3}): rpc median {amed:.4f}s  max {max(above):.4f}s")
    if bmed and amed and bmed > 0:
        print(f"  -> median rpc_seconds {amed / bmed:.1f}x higher after Orchard activation")
    if parse_all:
        print(f"parse_seconds: median {median(parse_all):.6f}s  max {max(parse_all):.6f}s"
              f"  ({'negligible vs RPC' if scale and median(parse_all) < scale / 20 else 'NOT negligible'})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
