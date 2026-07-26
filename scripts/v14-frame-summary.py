#!/usr/bin/env python3
"""v1.4.0 baseline: frame-trace log summarizer.

Parses `tracing`-style `frame` log lines emitted by
`weft_app::frame_trace::FrameTraceRecorder::finish` and computes p50/p95/avg
for every counter. Designed to be piped from `scripts/v14-perf-probe.sh`.

A typical line looks like (one line, fields space-separated):

    2026-07-26T... DEBUG weft_app::frame_trace: frame_id=1 reason=pty \
        layout_us=42 build_us=150 encode_us=80 cpu_total_us=272 \
        vertices=12 instances=1920 dirty_rows=0 session_blocks=0 \
        visible_blocks=0 bv_rows=0 cache_hits=0 cache_misses=0 \
        styled_lookups=0 styled_paint_us=0 resident_bytes=123456 \
        grid_bg_instances=0 grid_glyph_instances=1920 grid_upload_bytes=122880 \
        styled_cache_hits=0 styled_cache_misses=0 styled_cache_bytes=0 \
        gpu_completions_this_frame=0 gpu_max_us=0 frame

The trailing ` frame` is the message; everything else is key=value pairs.
Unknown keys are ignored; missing keys default to 0.
"""

import argparse
import math
import re
import sys
from datetime import datetime


# Counters reported as microseconds (durations). p50/p95/avg in ms.
DURATION_KEYS_US = [
    "layout_us",
    "build_us",
    "encode_us",
    "cpu_total_us",
    "styled_paint_us",
    "gpu_max_us",
]

# Counters reported as raw integers. p50/p95/avg as-is.
COUNT_KEYS = [
    "vertices",
    "instances",
    "dirty_rows",
    "session_blocks",
    "visible_blocks",
    "bv_rows",
    "cache_hits",
    "cache_misses",
    "styled_lookups",
    "resident_bytes",
    "grid_bg_instances",
    "grid_glyph_instances",
    "grid_upload_bytes",
    "styled_cache_hits",
    "styled_cache_misses",
    "styled_cache_bytes",
]

KV_RE = re.compile(r"(\w+)=(\S+)")


def percentile(values, pct):
    """Nearest-rank percentile (matches performance_probe.rs)."""
    if not values:
        return 0.0
    s = sorted(values)
    idx = min(
        max(math.ceil(len(s) * pct / 100.0) - 1, 0),
        len(s) - 1,
    )
    return s[idx]


def parse_frame_line(line):
    """Return a dict of key→int for one `frame` log line, or None."""
    if not line.rstrip().endswith(" frame"):
        return None
    kv = {m.group(1): m.group(2) for m in KV_RE.finditer(line)}
    out = {}
    for k in DURATION_KEYS_US + COUNT_KEYS:
        v = kv.get(k)
        if v is None:
            out[k] = 0
            continue
        try:
            out[k] = int(v)
        except ValueError:
            out[k] = 0
    out["reason"] = kv.get("reason", "?")
    out["frame_id"] = int(kv.get("frame_id", "0") or "0")
    return out


def summarize(frames):
    """Compute p50/p95/avg for every counter across all frames."""
    summary = {}
    for k in DURATION_KEYS_US:
        vals = [f[k] for f in frames]
        summary[k] = {
            "p50_us": percentile(vals, 50),
            "p95_us": percentile(vals, 95),
            "avg_us": (sum(vals) / len(vals)) if vals else 0,
            "max_us": max(vals) if vals else 0,
            "unit": "us",
        }
    for k in COUNT_KEYS:
        vals = [f[k] for f in frames]
        summary[k] = {
            "p50": percentile(vals, 50),
            "p95": percentile(vals, 95),
            "avg": (sum(vals) / len(vals)) if vals else 0,
            "max": max(vals) if vals else 0,
        }
    # Reason breakdown.
    reasons = {}
    for f in frames:
        reasons[f["reason"]] = reasons.get(f["reason"], 0) + 1
    summary["reasons"] = reasons
    return summary


def fmt_us(v):
    if v >= 1000:
        return f"{v / 1000:.3f} ms"
    return f"{v:.0f} us"


def fmt_bytes(v):
    if v >= 1024 * 1024:
        return f"{v / (1024 * 1024):.2f} MiB"
    if v >= 1024:
        return f"{v / 1024:.2f} KiB"
    if isinstance(v, float):
        return f"{v:.0f} B"
    return f"{v} B"


def render_markdown(summary, frames, meta):
    lines = []
    lines.append(f"# {meta['title']}")
    lines.append("")
    lines.append("## Environment")
    lines.append(f"- commit: `{meta['commit']}`")
    lines.append(f"- macOS: `{meta['os']}`")
    lines.append(f"- scale hint: `{meta['scale_hint']}`")
    lines.append(f"- warmup: {meta['warmup_secs']}s")
    lines.append(f"- sample: {meta['sample_secs']}s")
    lines.append(f"- frames captured: {len(frames)}")
    lines.append(f"- generated: {datetime.now().isoformat(timespec='seconds')}")
    lines.append("")
    reasons = summary["reasons"]
    if reasons:
        rstr = ", ".join(f"{k}={v}" for k, v in sorted(reasons.items()))
        lines.append(f"- frame reasons: {rstr}")
        lines.append("")

    lines.append("## Duration counters (p50 / p95 / avg / max)")
    lines.append("")
    lines.append("| counter | p50 | p95 | avg | max |")
    lines.append("|---|---|---|---|---|")
    for k in DURATION_KEYS_US:
        s = summary[k]
        lines.append(
            f"| {k} | {fmt_us(s['p50_us'])} | {fmt_us(s['p95_us'])} | "
            f"{fmt_us(s['avg_us'])} | {fmt_us(s['max_us'])} |"
        )
    lines.append("")

    lines.append("## Count counters (p50 / p95 / avg / max)")
    lines.append("")
    lines.append("| counter | p50 | p95 | avg | max |")
    lines.append("|---|---|---|---|---|")
    for k in COUNT_KEYS:
        s = summary[k]
        if "bytes" in k or "resident" in k:
            lines.append(
                f"| {k} | {fmt_bytes(s['p50'])} | {fmt_bytes(s['p95'])} | "
                f"{fmt_bytes(s['avg'])} | {fmt_bytes(s['max'])} |"
            )
        else:
            lines.append(
                f"| {k} | {s['p50']:.0f} | {s['p95']:.0f} | "
                f"{s['avg']:.0f} | {s['max']:.0f} |"
            )
    lines.append("")

    lines.append("## v1.4 GO/NO-GO checkpoints")
    lines.append("")
    sp = summary["styled_paint_us"]
    cpu = summary["cpu_total_us"]
    sp_p95_ms = sp["p95_us"] / 1000.0
    cpu_p95_ms = cpu["p95_us"] / 1000.0
    sp_share = (sp["p95_us"] / cpu["p95_us"] * 100.0) if cpu["p95_us"] else 0.0
    lines.append(f"- styled_paint_us p95: **{sp_p95_ms:.3f} ms** (v1.4.1 GO if ≥ 1.0 ms)")
    lines.append(
        f"- styled_paint_us share of cpu_total_us p95: **{sp_share:.1f}%** "
        f"(v1.4.1 GO if ≥ 15%)"
    )
    lines.append(f"- cpu_total_us p95: **{cpu_p95_ms:.3f} ms**")
    lines.append("")
    return "\n".join(lines)


def main():
    ap = argparse.ArgumentParser(description="v1.4 frame-trace summarizer")
    ap.add_argument("--log", required=True, help="path to captured stderr log")
    ap.add_argument("--commit", default="unknown")
    ap.add_argument("--os", default="unknown")
    ap.add_argument("--sample-secs", type=int, default=30)
    ap.add_argument("--warmup-secs", type=int, default=5)
    ap.add_argument("--scale-hint", default="unknown")
    ap.add_argument("--title", default="Frame baseline")
    args = ap.parse_args()

    frames = []
    with open(args.log, "r", encoding="utf-8", errors="replace") as f:
        for line in f:
            parsed = parse_frame_line(line)
            if parsed is not None:
                frames.append(parsed)

    if not frames:
        print("v14-frame-summary: no `frame` lines found in log", file=sys.stderr)
        sys.exit(1)

    summary = summarize(frames)
    meta = {
        "title": args.title,
        "commit": args.commit,
        "os": args.os,
        "scale_hint": args.scale_hint,
        "sample_secs": args.sample_secs,
        "warmup_secs": args.warmup_secs,
    }
    print(render_markdown(summary, frames, meta))


if __name__ == "__main__":
    main()
