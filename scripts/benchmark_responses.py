#!/usr/bin/env python3
"""Minimal speed probe for POST /v1/responses. Prints one JSON line."""

from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor, as_completed
import json
import statistics
import time
import urllib.error
import urllib.request

PROMPT = (
    "Explain in 6 concise bullet points why structured request logging helps "
    "debug API proxies. Keep the whole answer between 90 and 110 tokens total."
)

DEFAULT_MODELS = [
    "gpt-5.4",
    "gpt-5.3-codex",
    "gpt-5.3-codex-spark",
]
DEFAULT_REASONING_EFFORTS = ["low", "medium", "high", "xhigh"]
DEFAULT_SERVICE_TIERS = ["omit", "priority"]


def parse_csv(value: str | None) -> list[str]:
    if not value:
        return []
    return [item.strip() for item in value.split(",") if item.strip()]


def parse_list_arg(value: str | None, default: list[str]) -> list[str]:
    return list(dict.fromkeys(parse_csv(value) or default))


def normalize_service_tier(value: str | None) -> str | None:
    if not value:
        return None
    lowered = value.lower()
    return None if lowered == "omit" else lowered


def describe_http_error(exc: urllib.error.HTTPError) -> str:
    status = exc.code
    message = exc.reason or "request failed"
    try:
        body = exc.read().decode().strip()[:500]
        if body:
            return f"HTTP {status} {message}: {body}"
    except (AttributeError, LookupError, TypeError, UnicodeDecodeError, ValueError):
        pass
    return f"HTTP {status} {message}"


def describe_error(exc: Exception) -> str:
    if isinstance(exc, urllib.error.HTTPError):
        return describe_http_error(exc)
    if isinstance(exc, urllib.error.URLError):
        return f"URLError: {exc.reason}"
    return f"{exc.__class__.__name__}: {exc}"


def percentile_from_sorted(ordered: list[float], fraction: float) -> float:
    if not ordered:
        return 0.0
    if len(ordered) == 1:
        return ordered[0]
    rank = (len(ordered) - 1) * fraction
    lower = int(rank)
    upper = min(lower + 1, len(ordered) - 1)
    return ordered[lower] + (ordered[upper] - ordered[lower]) * (rank - lower)


def summarize_rates(
    samples: list[float],
) -> tuple[float | None, float | None, float | None, float]:
    if not samples:
        return None, None, None, 0.0
    ordered = sorted(samples)
    return (
        round(statistics.fmean(samples), 3),
        round(percentile_from_sorted(ordered, 0.50), 3),
        round(percentile_from_sorted(ordered, 0.95), 3),
        round(statistics.stdev(samples), 3) if len(samples) > 1 else 0.0,
    )


def summarize_latencies(samples: list[float]) -> tuple[float | None, float | None]:
    if not samples:
        return None, None
    ordered = sorted(samples)
    return (
        round(statistics.fmean(samples), 1),
        round(percentile_from_sorted(ordered, 0.95), 1),
    )


def run_once(
    url: str, key: str, payload_data: bytes, timeout: int
) -> tuple[float, int]:
    req = urllib.request.Request(
        url,
        data=payload_data,
        headers={
            "authorization": f"Bearer {key}",
            "content-type": "application/json",
        },
        method="POST",
    )
    started = time.perf_counter()
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        payload = json.loads(resp.read())
    elapsed = time.perf_counter() - started
    usage = payload.get("usage") or {}
    tokens = int(usage.get("output_tokens") or 0)
    return elapsed, tokens


def run_attempt(
    url: str, api_key: str, payload_data: bytes, timeout: int
) -> tuple[float | None, int | None, str | None, int | None]:
    try:
        elapsed, tokens = run_once(url, api_key, payload_data, timeout)
        return elapsed * 1000.0, tokens, None, None
    except (
        urllib.error.HTTPError,
        urllib.error.URLError,
        TimeoutError,
        OSError,
        ValueError,
        json.JSONDecodeError,
    ) as exc:
        return None, None, describe_error(exc), getattr(exc, "code", None)


def run_combination(
    url: str,
    api_key: str,
    model: str,
    reasoning: str,
    service_tier: str,
    iterations: int,
    concurrency: int,
    timeout: int,
) -> dict:
    request_body: dict = {
        "model": model,
        "input": PROMPT,
        "stream": False,
        "reasoning": {"effort": reasoning},
    }
    requested_service_tier = normalize_service_tier(service_tier)
    if requested_service_tier is not None:
        request_body["service_tier"] = requested_service_tier

    payload_data = json.dumps(request_body).encode()
    latencies: list[float] = []
    tok_per_s: list[float] = []
    failures = 0
    last_error = None
    last_status = None

    worker_count = min(concurrency, iterations)
    started = time.perf_counter()
    with ThreadPoolExecutor(max_workers=worker_count) as executor:
        futures = [
            executor.submit(run_attempt, url, api_key, payload_data, timeout)
            for _ in range(iterations)
        ]
        for future in as_completed(futures):
            elapsed_ms, tokens, error, status = future.result()
            if error is not None:
                failures += 1
                last_error = error
                last_status = status
                continue
            if elapsed_ms is None or tokens is None:
                continue
            latencies.append(elapsed_ms)
            if elapsed_ms > 0 and tokens > 0:
                tok_per_s.append(tokens / (elapsed_ms / 1000.0))
    wall_time_s = time.perf_counter() - started

    successes = iterations - failures
    avg_tps, p50_tps, p95_tps, stdev_tps = summarize_rates(tok_per_s)
    avg_latency_ms, p95_latency_ms = summarize_latencies(latencies)

    return {
        "model": model,
        "reasoning": reasoning,
        "service_tier": requested_service_tier or "omit",
        "iterations": iterations,
        "concurrency": worker_count,
        "failures": failures,
        "successes": successes,
        "wall_time_ms": round(wall_time_s * 1000.0, 1),
        "request_rate_rps": round(iterations / wall_time_s, 3)
        if wall_time_s > 0
        else None,
        "success_rate_rps": round(successes / wall_time_s, 3)
        if wall_time_s > 0
        else None,
        "avg_tps": avg_tps,
        "p50_tps": p50_tps,
        "p95_tps": p95_tps,
        "stdev_tps": stdev_tps,
        "avg_latency_ms": avg_latency_ms,
        "p95_latency_ms": p95_latency_ms,
        "last_error": last_error,
        "last_http_status": last_status,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--proxy-url", required=True, help="base URL, no trailing slash"
    )
    parser.add_argument("--api-key", required=True)
    parser.add_argument(
        "--model",
        default=",".join(DEFAULT_MODELS),
        help="comma-separated model ids (default: gpt-5.4,gpt-5.3-codex,gpt-5.3-codex-spark)",
    )
    parser.add_argument(
        "--reasoning",
        default=",".join(DEFAULT_REASONING_EFFORTS),
        help="comma-separated reasoning effort values, e.g. low,medium,high,xhigh",
    )
    parser.add_argument(
        "--service-tier",
        default=",".join(DEFAULT_SERVICE_TIERS),
        help="comma-separated service_tier values; use 'omit' for no service_tier",
    )
    parser.add_argument("--iterations", type=int, default=5)
    parser.add_argument(
        "--concurrency",
        type=int,
        default=1,
        help="max in-flight requests per combination (default: 1)",
    )
    parser.add_argument("--timeout", type=int, default=60)
    args = parser.parse_args()
    if args.iterations < 1:
        parser.error("--iterations must be >= 1")
    if args.concurrency < 1:
        parser.error("--concurrency must be >= 1")

    url = f"{args.proxy_url.rstrip('/')}/v1/responses"
    models = parse_list_arg(args.model, DEFAULT_MODELS)
    reasoning_levels = parse_list_arg(args.reasoning, DEFAULT_REASONING_EFFORTS)
    service_tiers = parse_list_arg(args.service_tier, DEFAULT_SERVICE_TIERS)

    total_combinations = 0
    total_failures = 0
    total_requests = 0
    for model in models:
        for reasoning in reasoning_levels:
            for service_tier in service_tiers:
                summary = run_combination(
                    url,
                    args.api_key,
                    model,
                    reasoning,
                    service_tier,
                    args.iterations,
                    args.concurrency,
                    args.timeout,
                )
                print(json.dumps(summary))
                total_combinations += 1
                total_failures += summary["failures"]
                total_requests += summary["iterations"]

    if total_combinations:
        print(
            json.dumps(
                {
                    "total_combinations": total_combinations,
                    "total_failures": total_failures,
                    "total_requests": total_requests,
                }
            )
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
