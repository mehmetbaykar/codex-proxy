#!/usr/bin/env python3
"""Minimal speed probe for POST /v1/responses. Prints one JSON line."""
from __future__ import annotations

import argparse
import json
import statistics
import time
import urllib.error
import urllib.request

PROMPT = (
    "Explain in 6 concise bullet points why structured request logging helps "
    "debug API proxies. Keep the whole answer between 90 and 110 tokens total."
)


def percentile(values: list[float], fraction: float) -> float:
    ordered = sorted(values)
    if not ordered:
        return 0.0
    if len(ordered) == 1:
        return ordered[0]
    rank = (len(ordered) - 1) * fraction
    lower = int(rank)
    upper = min(lower + 1, len(ordered) - 1)
    return ordered[lower] + (ordered[upper] - ordered[lower]) * (rank - lower)


def run_once(url: str, key: str, body: dict, timeout: int) -> tuple[float, int]:
    data = json.dumps(body).encode()
    req = urllib.request.Request(
        url,
        data=data,
        headers={
            "authorization": f"Bearer {key}",
            "content-type": "application/json",
        },
        method="POST",
    )
    started = time.perf_counter()
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        payload = json.loads(resp.read().decode())
    elapsed = time.perf_counter() - started
    usage = payload.get("usage") or {}
    tokens = int(usage.get("output_tokens") or 0)
    return elapsed, tokens


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--proxy-url", required=True, help="base URL, no trailing slash")
    parser.add_argument("--api-key", required=True)
    parser.add_argument("--model", required=True)
    parser.add_argument("--reasoning", default=None, help="low/medium/high")
    parser.add_argument("--service-tier", default=None, help="default/priority")
    parser.add_argument("--iterations", type=int, default=5)
    parser.add_argument("--timeout", type=int, default=60)
    args = parser.parse_args()

    body: dict = {"model": args.model, "input": PROMPT, "stream": False}
    if args.reasoning:
        body["reasoning"] = {"effort": args.reasoning}
    if args.service_tier:
        body["service_tier"] = args.service_tier

    url = f"{args.proxy_url.rstrip('/')}/v1/responses"
    latencies: list[float] = []
    tok_per_s: list[float] = []
    failures = 0
    for _ in range(args.iterations):
        try:
            elapsed, tokens = run_once(url, args.api_key, body, args.timeout)
            latencies.append(elapsed * 1000.0)
            if elapsed > 0 and tokens > 0:
                tok_per_s.append(tokens / elapsed)
        except (urllib.error.URLError, urllib.error.HTTPError, TimeoutError, OSError):
            failures += 1

    summary = {
        "model": args.model,
        "reasoning": args.reasoning,
        "service_tier": args.service_tier,
        "iterations": args.iterations,
        "failures": failures,
        "avg_tps": round(statistics.fmean(tok_per_s), 3) if tok_per_s else None,
        "p50_tps": round(percentile(tok_per_s, 0.50), 3) if tok_per_s else None,
        "p95_tps": round(percentile(tok_per_s, 0.95), 3) if tok_per_s else None,
        "stdev_tps": round(statistics.stdev(tok_per_s), 3) if len(tok_per_s) > 1 else 0.0,
        "avg_latency_ms": round(statistics.fmean(latencies), 1) if latencies else None,
        "p95_latency_ms": round(percentile(latencies, 0.95), 1) if latencies else None,
    }
    print(json.dumps(summary))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
