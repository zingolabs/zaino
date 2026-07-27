#!/usr/bin/env python3
"""Parse sync-bench JSON tracing logs and display metrics.

Usage:
    kubectl logs job/sync-bench -n golden-mainnet | python parse-logs.py
    kubectl logs -f job/sync-bench -n golden-mainnet | python parse-logs.py --live
    python parse-logs.py < logs.json
    python parse-logs.py --compare <(kubectl logs job/A -n ns) <(kubectl logs job/B -n ns)

Flags:
    --live       streaming mode, print rolling updates
    --compare    compare two log files side by side
    --csv        dump raw data as CSV to stdout
    --summary    only print the final summary (no rolling output)
"""

import json
import sys
import argparse
from collections import defaultdict
from dataclasses import dataclass, field
from datetime import datetime
from typing import TextIO


# ---------------------------------------------------------------------------
# Data model
# ---------------------------------------------------------------------------

@dataclass
class BatchCommit:
    batch: int
    height: int
    op_count: int
    timestamp: datetime
    task_count: int | None = None


@dataclass
class MergeEvent:
    index: str
    batch: int
    duration_us: float  # microseconds
    timestamp: datetime


@dataclass
class ProvisionerProgress:
    sent: int
    total: int
    elapsed_secs: float
    blocks_per_sec: float
    timestamp: datetime


@dataclass
class Metrics:
    commits: list[BatchCommit] = field(default_factory=list)
    merges: list[MergeEvent] = field(default_factory=list)
    provisioner: list[ProvisionerProgress] = field(default_factory=list)


# ---------------------------------------------------------------------------
# Parsing
# ---------------------------------------------------------------------------

def parse_duration_str(s: str) -> float:
    """Parse tracing duration like '41.2ms', '3.40s', '520us' to microseconds."""
    s = s.strip()
    if s.endswith("ms"):
        return float(s[:-2]) * 1000
    elif s.endswith("µs") or s.endswith("us"):
        suffix_len = 2 if s.endswith("us") else len("µs")
        return float(s[:-suffix_len])
    elif s.endswith("s"):
        return float(s[:-1]) * 1_000_000
    return 0.0


def parse_timestamp(ts: str) -> datetime:
    # Handle fractional seconds of varying length
    if "." in ts:
        base, frac = ts.split(".")
        frac = frac.rstrip("Z")
        frac = frac[:6].ljust(6, "0")
        return datetime.fromisoformat(f"{base}.{frac}+00:00")
    return datetime.fromisoformat(ts.replace("Z", "+00:00"))


def parse_line(line: str, metrics: Metrics) -> str | None:
    """Parse one JSON line, append to metrics. Returns event type or None."""
    line = line.strip()
    if not line:
        return None
    try:
        obj = json.loads(line)
    except json.JSONDecodeError:
        return None

    fields = obj.get("fields", {})
    msg = fields.get("message", "")
    ts = parse_timestamp(obj["timestamp"])

    # Batch commit
    if msg == "atomic batch commit":
        task_count = None
        for s in obj.get("spans", []):
            if "task_count" in s:
                task_count = s["task_count"]
                break
        metrics.commits.append(BatchCommit(
            batch=fields["batch"],
            height=fields["committed_height"],
            op_count=fields["op_count"],
            timestamp=ts,
            task_count=task_count,
        ))
        return "commit"

    # Merge/persist span close
    span = obj.get("span", {})
    if span.get("name") == "merge_persist" and msg == "close":
        dur = parse_duration_str(fields.get("time.busy", "0us"))
        metrics.merges.append(MergeEvent(
            index=span.get("index", "?"),
            batch=span.get("batch", -1),
            duration_us=dur,
            timestamp=ts,
        ))
        return "merge"

    # Provisioner progress
    if msg == "provisioner progress":
        metrics.provisioner.append(ProvisionerProgress(
            sent=fields["sent"],
            total=fields["total"],
            elapsed_secs=float(fields["elapsed_secs"]),
            blocks_per_sec=float(fields["blocks_per_sec"]),
            timestamp=ts,
        ))
        return "progress"

    return None


def parse_stream(f: TextIO) -> Metrics:
    metrics = Metrics()
    for line in f:
        parse_line(line, metrics)
    return metrics


# ---------------------------------------------------------------------------
# Analysis
# ---------------------------------------------------------------------------

def percentile(values: list[float], p: float) -> float:
    if not values:
        return 0.0
    sorted_v = sorted(values)
    k = (len(sorted_v) - 1) * p / 100
    f = int(k)
    c = f + 1
    if c >= len(sorted_v):
        return sorted_v[f]
    return sorted_v[f] + (k - f) * (sorted_v[c] - sorted_v[f])


def height_band(h: int) -> str:
    """Group height into 500k bands."""
    band = (h // 500_000) * 500_000
    return f"{band // 1000}k-{(band + 500_000) // 1000}k"


def compute_throughput(commits: list[BatchCommit]) -> list[dict]:
    """Compute per-batch throughput from consecutive commits."""
    rows = []
    for i in range(1, len(commits)):
        prev, cur = commits[i - 1], commits[i]
        dt = (cur.timestamp - prev.timestamp).total_seconds()
        if dt <= 0:
            continue
        dh = cur.height - prev.height
        rows.append({
            "batch": cur.batch,
            "height": cur.height,
            "blocks": dh,
            "duration_s": dt,
            "blocks_per_sec": dh / dt,
            "op_count": cur.op_count,
            "task_count": cur.task_count,
            "timestamp": cur.timestamp,
        })
    return rows


def merge_stats_by_index(merges: list[MergeEvent]) -> dict[str, dict]:
    by_index: dict[str, list[float]] = defaultdict(list)
    for m in merges:
        by_index[m.index].append(m.duration_us)
    stats = {}
    for idx, durations in sorted(by_index.items()):
        stats[idx] = {
            "count": len(durations),
            "mean_ms": sum(durations) / len(durations) / 1000,
            "p50_ms": percentile(durations, 50) / 1000,
            "p95_ms": percentile(durations, 95) / 1000,
            "max_ms": max(durations) / 1000,
        }
    return stats


def throughput_by_band(rows: list[dict]) -> dict[str, dict]:
    bands: dict[str, list[float]] = defaultdict(list)
    for r in rows:
        bands[height_band(r["height"])].append(r["blocks_per_sec"])
    stats = {}
    for band, values in sorted(bands.items()):
        stats[band] = {
            "count": len(values),
            "mean": sum(values) / len(values),
            "p50": percentile(values, 50),
            "p95": percentile(values, 95),
            "min": min(values),
            "max": max(values),
        }
    return stats


# ---------------------------------------------------------------------------
# Display
# ---------------------------------------------------------------------------

BOLD = "\033[1m"
DIM = "\033[2m"
RESET = "\033[0m"
CYAN = "\033[36m"
GREEN = "\033[32m"
YELLOW = "\033[33m"


def print_header(title: str):
    print(f"\n{BOLD}{CYAN}{'=' * 60}{RESET}")
    print(f"{BOLD}{CYAN}  {title}{RESET}")
    print(f"{BOLD}{CYAN}{'=' * 60}{RESET}")


def print_throughput_table(rows: list[dict], last_n: int = 0):
    """Print throughput table. If last_n > 0, only show last N rows."""
    display = rows[-last_n:] if last_n else rows
    if not display:
        print("  (no data)")
        return

    print(f"  {'batch':>6}  {'height':>10}  {'blocks':>6}  {'dur(s)':>7}  {'blk/s':>8}  {'ops':>8}  {'tasks':>5}")
    print(f"  {'-' * 6}  {'-' * 10}  {'-' * 6}  {'-' * 7}  {'-' * 8}  {'-' * 8}  {'-' * 5}")
    for r in display:
        tc = str(r["task_count"]) if r["task_count"] is not None else "?"
        bps = r["blocks_per_sec"]
        color = GREEN if bps > 1000 else YELLOW if bps > 500 else ""
        end = RESET if color else ""
        print(f"  {r['batch']:>6}  {r['height']:>10,}  {r['blocks']:>6}  {r['duration_s']:>7.1f}  {color}{bps:>8.0f}{end}  {r['op_count']:>8,}  {tc:>5}")


def print_band_summary(bands: dict[str, dict]):
    if not bands:
        print("  (no data)")
        return

    print(f"  {'band':>12}  {'n':>5}  {'mean':>8}  {'p50':>8}  {'p95':>8}  {'min':>8}  {'max':>8}")
    print(f"  {'-' * 12}  {'-' * 5}  {'-' * 8}  {'-' * 8}  {'-' * 8}  {'-' * 8}  {'-' * 8}")
    for band, s in bands.items():
        print(f"  {band:>12}  {s['count']:>5}  {s['mean']:>8.0f}  {s['p50']:>8.0f}  {s['p95']:>8.0f}  {s['min']:>8.0f}  {s['max']:>8.0f}")


def print_merge_stats(stats: dict[str, dict]):
    if not stats:
        print("  (no data)")
        return

    print(f"  {'index':<22}  {'n':>6}  {'mean':>8}  {'p50':>8}  {'p95':>8}  {'max':>8}")
    print(f"  {'-' * 22}  {'-' * 6}  {'-' * 8}  {'-' * 8}  {'-' * 8}  {'-' * 8}")
    for idx, s in stats.items():
        print(f"  {idx:<22}  {s['count']:>6}  {s['mean_ms']:>7.1f}ms  {s['p50_ms']:>7.1f}ms  {s['p95_ms']:>7.1f}ms  {s['max_ms']:>7.1f}ms")


def print_provisioner_stats(prov: list[ProvisionerProgress]):
    if not prov:
        return
    print_header("Provisioner Progress")
    last = prov[-1]
    print(f"  sent: {last.sent:,} / {last.total:,}  ({last.sent / last.total * 100:.1f}%)")
    print(f"  avg provisioner rate: {last.blocks_per_sec:.0f} blocks/s")
    if len(prov) >= 2:
        rates = [p.blocks_per_sec for p in prov]
        print(f"  rate range: {min(rates):.0f} - {max(rates):.0f} blocks/s")


def print_ascii_sparkline(values: list[float], width: int = 50, label: str = ""):
    """Simple ASCII sparkline."""
    if not values:
        return
    vmin, vmax = min(values), max(values)
    spread = vmax - vmin if vmax > vmin else 1
    bars = " _.-~*"
    line = ""
    # Downsample if needed
    if len(values) > width:
        step = len(values) / width
        sampled = [values[int(i * step)] for i in range(width)]
    else:
        sampled = values
    for v in sampled:
        idx = int((v - vmin) / spread * (len(bars) - 1))
        line += bars[idx]
    print(f"  {label} [{vmin:.0f} .. {vmax:.0f}]")
    print(f"  {DIM}{line}{RESET}")


def print_full_report(metrics: Metrics, label: str = ""):
    title = f"Sync Bench Analysis"
    if label:
        title += f" ({label})"
    print_header(title)

    if metrics.commits:
        first = metrics.commits[0]
        last = metrics.commits[-1]
        total_time = (last.timestamp - first.timestamp).total_seconds()
        total_blocks = last.height - first.height
        print(f"  height range: {first.height:,} -> {last.height:,}")
        print(f"  batches: {len(metrics.commits)}")
        print(f"  wall time: {total_time:.0f}s ({total_time / 60:.1f}m)")
        if total_time > 0:
            print(f"  overall: {total_blocks / total_time:.0f} blocks/s")

    rows = compute_throughput(metrics.commits)

    # Throughput by height band
    print_header("Throughput by Chain Height (blocks/s)")
    bands = throughput_by_band(rows)
    print_band_summary(bands)

    # Throughput sparkline
    if rows:
        print_header("Throughput Over Time")
        print_ascii_sparkline([r["blocks_per_sec"] for r in rows], label="blocks/s")
        print()
        print_ascii_sparkline([r["op_count"] for r in rows], label="ops/batch")

    # Task count evolution
    task_counts = [r["task_count"] for r in rows if r["task_count"] is not None]
    if task_counts:
        print_header("Parallelism (task_count)")
        print_ascii_sparkline(task_counts, label="tasks")
        tc_vals = sorted(set(task_counts))
        print(f"  values seen: {', '.join(str(v) for v in tc_vals)}")

    # Per-index merge stats
    print_header("Merge+Persist Duration by Index")
    mstats = merge_stats_by_index(metrics.merges)
    print_merge_stats(mstats)

    # Merge duration sparkline per index (top 3 slowest)
    if mstats:
        slowest = sorted(mstats.items(), key=lambda x: x[1]["mean_ms"], reverse=True)[:3]
        print_header("Merge Duration Over Time (top 3 slowest)")
        by_index: dict[str, list[float]] = defaultdict(list)
        for m in metrics.merges:
            by_index[m.index].append(m.duration_us / 1000)
        for idx, _ in slowest:
            if idx in by_index:
                print_ascii_sparkline(by_index[idx], label=f"{idx} (ms)")

    # Provisioner
    print_provisioner_stats(metrics.provisioner)

    # Recent batches
    print_header("Recent Batches (last 10)")
    print_throughput_table(rows, last_n=10)

    print()


def print_comparison(metrics_a: Metrics, metrics_b: Metrics, label_a: str, label_b: str):
    """Side-by-side comparison of two runs."""
    print_header(f"Comparison: {label_a} vs {label_b}")

    for label, m in [(label_a, metrics_a), (label_b, metrics_b)]:
        if m.commits:
            first, last = m.commits[0], m.commits[-1]
            total_time = (last.timestamp - first.timestamp).total_seconds()
            total_blocks = last.height - first.height
            rate = total_blocks / total_time if total_time > 0 else 0
            print(f"  {label}:")
            print(f"    height: {first.height:,} -> {last.height:,}")
            print(f"    wall time: {total_time:.0f}s, overall: {rate:.0f} blocks/s")

    # Compare throughput at matching height bands
    rows_a = compute_throughput(metrics_a.commits)
    rows_b = compute_throughput(metrics_b.commits)
    bands_a = throughput_by_band(rows_a)
    bands_b = throughput_by_band(rows_b)

    all_bands = sorted(set(bands_a.keys()) | set(bands_b.keys()))
    if all_bands:
        print(f"\n  {'band':>12}  {'A mean':>8}  {'B mean':>8}  {'delta':>8}  {'ratio':>6}")
        print(f"  {'-' * 12}  {'-' * 8}  {'-' * 8}  {'-' * 8}  {'-' * 6}")
        for band in all_bands:
            a_mean = bands_a.get(band, {}).get("mean", 0)
            b_mean = bands_b.get(band, {}).get("mean", 0)
            delta = b_mean - a_mean
            ratio = b_mean / a_mean if a_mean > 0 else 0
            a_str = f"{a_mean:.0f}" if a_mean else "-"
            b_str = f"{b_mean:.0f}" if b_mean else "-"
            d_str = f"{delta:+.0f}" if a_mean and b_mean else "-"
            r_str = f"{ratio:.2f}x" if a_mean and b_mean else "-"
            print(f"  {band:>12}  {a_str:>8}  {b_str:>8}  {d_str:>8}  {r_str:>6}")

    # Compare merge stats
    mstats_a = merge_stats_by_index(metrics_a.merges)
    mstats_b = merge_stats_by_index(metrics_b.merges)
    all_indexes = sorted(set(mstats_a.keys()) | set(mstats_b.keys()))
    if all_indexes:
        print(f"\n  {'index':<22}  {'A p50':>8}  {'B p50':>8}  {'delta':>8}")
        print(f"  {'-' * 22}  {'-' * 8}  {'-' * 8}  {'-' * 8}")
        for idx in all_indexes:
            a_p50 = mstats_a.get(idx, {}).get("p50_ms", 0)
            b_p50 = mstats_b.get(idx, {}).get("p50_ms", 0)
            delta = b_p50 - a_p50
            a_str = f"{a_p50:.1f}ms" if a_p50 else "-"
            b_str = f"{b_p50:.1f}ms" if b_p50 else "-"
            d_str = f"{delta:+.1f}ms" if a_p50 and b_p50 else "-"
            print(f"  {idx:<22}  {a_str:>8}  {b_str:>8}  {d_str:>8}")

    print()


def live_mode(f: TextIO):
    """Streaming mode: print rolling updates as data arrives."""
    metrics = Metrics()
    commit_count = 0
    for line in f:
        event = parse_line(line, metrics)
        if event == "commit" and metrics.commits:
            commit_count += 1
            c = metrics.commits[-1]
            rows = compute_throughput(metrics.commits)
            if rows:
                r = rows[-1]
                tc = r["task_count"] if r["task_count"] is not None else "?"
                print(
                    f"  batch {c.batch:>5}  "
                    f"height {c.height:>10,}  "
                    f"{r['blocks_per_sec']:>6.0f} blk/s  "
                    f"{r['duration_s']:>5.1f}s  "
                    f"ops={c.op_count:>8,}  "
                    f"tasks={tc}",
                    flush=True,
                )
            # Print summary every 100 commits
            if commit_count % 100 == 0:
                print_full_report(metrics, label="interim")

    if metrics.commits:
        print_full_report(metrics, label="final")


def csv_mode(metrics: Metrics):
    """Dump raw throughput data as CSV."""
    rows = compute_throughput(metrics.commits)
    print("batch,height,blocks,duration_s,blocks_per_sec,op_count,task_count")
    for r in rows:
        tc = r["task_count"] if r["task_count"] is not None else ""
        print(f"{r['batch']},{r['height']},{r['blocks']},{r['duration_s']:.3f},{r['blocks_per_sec']:.1f},{r['op_count']},{tc}")


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(description="Parse sync-bench JSON tracing logs")
    parser.add_argument("--live", action="store_true", help="Streaming mode with rolling updates")
    parser.add_argument("--csv", action="store_true", help="Output raw data as CSV")
    parser.add_argument("--summary", action="store_true", help="Only print summary, no rolling table")
    parser.add_argument("--compare", nargs=2, metavar=("FILE_A", "FILE_B"), help="Compare two log files")
    parser.add_argument("--last", type=int, default=20, help="Number of recent batches to show (default: 20)")
    parser.add_argument("files", nargs="*", help="Log files to parse (default: stdin)")
    args = parser.parse_args()

    if args.compare:
        with open(args.compare[0]) as fa, open(args.compare[1]) as fb:
            ma = parse_stream(fa)
            mb = parse_stream(fb)
        print_comparison(ma, mb, args.compare[0], args.compare[1])
        # Also print individual reports
        print_full_report(ma, label=args.compare[0])
        print_full_report(mb, label=args.compare[1])
        return

    if args.live:
        f = open(args.files[0]) if args.files else sys.stdin
        live_mode(f)
        return

    # Batch mode
    if args.files:
        for path in args.files:
            with open(path) as f:
                metrics = parse_stream(f)
            if args.csv:
                csv_mode(metrics)
            else:
                print_full_report(metrics, label=path)
    else:
        metrics = parse_stream(sys.stdin)
        if args.csv:
            csv_mode(metrics)
        else:
            print_full_report(metrics)


if __name__ == "__main__":
    main()
