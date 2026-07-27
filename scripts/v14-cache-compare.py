#!/usr/bin/env python3
"""Compare v1.4 pre-cache and post-cache loaded probe reports."""

import argparse
import re
from pathlib import Path


def duration_us(value: str) -> float:
    number, unit = value.strip().split()
    scale = {"us": 1.0, "ms": 1000.0}[unit]
    return float(number) * scale


def table_p95(text: str, counter: str) -> float:
    match = re.search(rf"^\| {re.escape(counter)} \| [^|]+\| ([^|]+)\|", text, re.MULTILINE)
    if not match:
        raise ValueError(f"missing p95 counter: {counter}")
    return duration_us(match.group(1)) if counter != "resident_bytes" else memory_mib(match.group(1))


def table_max(text: str, counter: str) -> float:
    match = re.search(
        rf"^\| {re.escape(counter)} \| [^|]+\| [^|]+\| [^|]+\| ([^|]+)\|",
        text,
        re.MULTILINE,
    )
    if not match:
        raise ValueError(f"missing max counter: {counter}")
    return memory_mib(match.group(1))


def memory_mib(value: str) -> float:
    number, unit = value.strip().split()
    scale = {"B": 1.0 / 1048576.0, "KiB": 1.0 / 1024.0, "MiB": 1.0}[unit]
    return float(number) * scale


def scalar(text: str, label: str) -> float:
    match = re.search(rf"^- {re.escape(label)}: ([0-9.]+)", text, re.MULTILINE)
    if not match:
        raise ValueError(f"missing scalar: {label}")
    return float(match.group(1))


def percent_reduction(before: float, after: float) -> float:
    return (before - after) / before * 100.0 if before else 0.0


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("baseline", type=Path)
    parser.add_argument("current", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()

    baseline = args.baseline.read_text()
    current = args.current.read_text()
    styled_before = table_p95(baseline, "styled_paint_us")
    styled_after = table_p95(current, "styled_paint_us")
    cpu_before = table_p95(baseline, "cpu_total_us")
    cpu_after = table_p95(current, "cpu_total_us")
    rss_before = table_max(baseline, "resident_bytes")
    rss_after = table_max(current, "resident_bytes")
    hit_rate = scalar(current, "Cache hit rate")
    cache_bytes = scalar(current, "Maximum cache bytes")

    styled_delta = percent_reduction(styled_before, styled_after)
    cpu_regression = (cpu_after - cpu_before) / cpu_before * 100.0
    rss_delta = rss_after - rss_before
    checks = [
        ("Cache hit rate", ">= 70%", f"{hit_rate:.2f}%", hit_rate >= 70.0),
        ("styled_paint_us p95 reduction", ">= 20%", f"{styled_delta:.1f}%", styled_delta >= 20.0),
        ("CPU frame p95 regression", "<= 5%", f"{cpu_regression:+.1f}%", cpu_regression <= 5.0),
        ("Cache resident bytes", "<= 16 MiB", f"{cache_bytes / 1048576.0:.3f} MiB", cache_bytes <= 16777216),
        ("Process RSS increase", "<= 25 MiB", f"{rss_delta:+.2f} MiB", rss_delta <= 25.0),
    ]
    passed = all(check[3] for check in checks)

    rows = "\n".join(
        f"| {name} | {threshold} | {actual} | {'PASS' if ok else 'FAIL'} |"
        for name, threshold, actual, ok in checks
    )
    report = f"""# v1.4.1 Loaded Cache Comparison

> Baseline: `{args.baseline}`
> Current: `{args.current}`

| Criterion | Threshold | Actual | Verdict |
|---|---:|---:|---|
{rows}

## Measurements

- `styled_paint_us` p95: {styled_before:.0f} us -> {styled_after:.0f} us
- `cpu_total_us` p95: {cpu_before:.0f} us -> {cpu_after:.0f} us
- Process RSS max: {rss_before:.2f} MiB -> {rss_after:.2f} MiB

## Decision

**{'PASS' if passed else 'FAIL'}**

The pre-cache startup gate is evaluated separately from these post-implementation exit criteria.
"""
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(report)
    print(report, end="")
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
