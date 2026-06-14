#!/usr/bin/env python3
"""Summarize treestate_probe CSV output so the per-height cost knee is obvious.

Reads the CSV emitted by the `treestate_probe` example and prints a per-row
table of the per-RPC timings, then a comparison of each RPC's median latency
below vs. at/above Orchard (NU5) activation, then an ASCII bar chart of
`treestate_seconds` by height.

The point of interest: does `treestate_seconds` climb after NU5 while
`blockcount_seconds` (a height-independent baseline) and `blockfetch_seconds`
stay flat? That isolates RPC #2 (commitment-tree fetch) from RPC #1 (block
fetch) and from general validator/RPC slowdown.

Column lookup is by header name, so it tolerates added/reordered columns.

Usage:
    treestate_probe ... > probe.csv
    ./treestate_probe_summary.py probe.csv
    # or pipe:
    treestate_probe ... | ./treestate_probe_summary.py
"""

import sys

NU5_HEIGHT = 1_687_104  # mainnet Orchard activation
BAR_WIDTH = 36


def median(values):
    if not values:
        return None
    ordered = sorted(values)
    mid = len(ordered) // 2
    if len(ordered) % 2:
        return ordered[mid]
    return (ordered[mid - 1] + ordered[mid]) / 2.0


def bar(value, scale):
    if scale <= 0 or value is None:
        return ""
    eighths = round((value / scale) * BAR_WIDTH * 8)
    full, rem = divmod(eighths, 8)
    return ("█" * full) + (" ▏▎▍▌▋▊▉"[rem] if rem else "")


def ms(value):
    return f"{value * 1000:.2f}" if value is not None else "—"


def main():
    source = open(sys.argv[1]) if len(sys.argv) > 1 else sys.stdin
    col = {}
    rows = []
    with source:
        for line in source:
            line = line.strip()
            if not line:
                continue
            parts = line.split(",")
            if line.startswith("height,"):
                col = {name: i for i, name in enumerate(parts)}
                continue
            if col:
                rows.append(parts)

    if not rows:
        print("no data rows found", file=sys.stderr)
        return 1

    def get(row, name):
        """Float value of named column, or None if absent/blank/unparsable."""
        idx = col.get(name)
        if idx is None or idx >= len(row) or row[idx] == "":
            return None
        try:
            return float(row[idx])
        except ValueError:
            return None

    def height_of(row):
        return int(row[col["height"]])

    def split(name):
        below = [get(r, name) for r in rows if height_of(r) < NU5_HEIGHT and get(r, name) is not None]
        above = [get(r, name) for r in rows if height_of(r) >= NU5_HEIGHT and get(r, name) is not None]
        return below, above

    errors = sum(1 for r in rows if get(r, "parse_seconds") is None)

    # --- per-row table ---
    hdr = (f"{'height':>9} {'bcount_ms':>9} {'block_ms':>9} {'tree_ms':>9} "
           f"{'parse_ms':>9} {'block_kb':>8} {'orchard':>11}")
    print(hdr)
    print("-" * len(hdr))
    for r in rows:
        bb = get(r, "block_bytes")
        orc = get(r, "orchard_size")
        block_kb = f"{bb / 1024:8.1f}" if bb is not None else f"{'—':>8}"
        orchard = f"{int(orc):11d}" if orc is not None else f"{'—':>11}"
        marker = "  <- NU5" if height_of(r) == NU5_HEIGHT else ""
        print(
            f"{height_of(r):9d} "
            f"{ms(get(r, 'blockcount_seconds')):>9} "
            f"{ms(get(r, 'blockfetch_seconds')):>9} "
            f"{ms(get(r, 'treestate_seconds')):>9} "
            f"{ms(get(r, 'parse_seconds')):>9} "
            f"{block_kb} {orchard}{marker}"
        )
    print("-" * len(hdr))
    print(f"rows: {len(rows)}   treestate errors/timeouts: {errors}")

    # --- below vs at/above NU5 comparison ---
    print("\nmedian latency below vs at/above NU5:")
    print(f"  {'rpc':<20} {'below':>10} {'at/above':>10} {'ratio':>8}")
    for name in ("blockcount_seconds", "blockfetch_seconds", "treestate_seconds", "parse_seconds"):
        below, above = split(name)
        bmed, amed = median(below), median(above)
        ratio = f"{amed / bmed:.1f}x" if (bmed and amed and bmed > 0) else "—"
        b = f"{bmed * 1000:.2f}ms" if bmed is not None else "—"
        a = f"{amed * 1000:.2f}ms" if amed is not None else "—"
        print(f"  {name:<20} {b:>10} {a:>10} {ratio:>8}")

    bb_below, bb_above = split("block_bytes")
    if bb_below or bb_above:
        mb, ma = median(bb_below) or 0, median(bb_above) or 0
        print(f"  {'block_bytes (KB)':<20} {mb / 1024:>9.1f} {ma / 1024:>9.1f}"
              f"   (sandblast bloat shows here if in range)")

    # --- treestate bar chart ---
    tree_present = [get(r, "treestate_seconds") for r in rows if get(r, "treestate_seconds") is not None]
    scale = max(tree_present) if tree_present else 0.0
    print("\ntreestate_seconds by height:")
    for r in rows:
        v = get(r, "treestate_seconds")
        marker = "  <- NU5" if height_of(r) == NU5_HEIGHT else ""
        print(f"  {height_of(r):9d} {ms(v):>9}ms  {bar(v, scale)}{marker}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
