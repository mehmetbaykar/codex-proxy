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
    except Exception:
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


def run_combination(
    url: str,
    api_key: str,
    model: str,
    reasoning: str,
    service_tier: str,
    iterations: int,
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

    for _ in range(iterations):
        try:
            elapsed, tokens = run_once(url, api_key, payload_data, timeout)
            latencies.append(elapsed * 1000.0)
            if elapsed > 0 and tokens > 0:
                tok_per_s.append(tokens / elapsed)
        except Exception as exc:
            failures += 1
            last_error = describe_error(exc)
            last_status = getattr(exc, "code", None)

    avg_tps, p50_tps, p95_tps, stdev_tps = summarize_rates(tok_per_s)
    avg_latency_ms, p95_latency_ms = summarize_latencies(latencies)

    return {
        "model": model,
        "reasoning": reasoning,
        "service_tier": requested_service_tier or "omit",
        "iterations": iterations,
        "failures": failures,
        "successes": iterations - failures,
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
    parser.add_argument("--timeout", type=int, default=60)
    args = parser.parse_args()

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
