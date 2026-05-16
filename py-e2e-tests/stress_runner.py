#!/usr/bin/env python3
"""Diem vao stress test - nhieu vong lap dong thoi, chay moi scenario basic/ + repair/

So song song an toan = max(1, so tai khoan / 2), stress mac dinh = so an toan + 1
"""

import argparse
import json
import sys
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from datetime import datetime
from typing import Any

from config import load_config
from runner import (
    load_scenarios, run_openai, run_anthropic,
    format_duration, print_report,
)
from openai import OpenAI
from anthropic import Anthropic
import httpx

def main():
    config = load_config()
    safe = config["safe_concurrency"]
    api_key = config["api_key"]
    stress_parallel = safe + 1

    parser = argparse.ArgumentParser(description="Stress test e2e")
    parser.add_argument("--iterations", type=int, default=3, help="So vong lap moi scenario (mac dinh: 3)")
    parser.add_argument("--parallel", type=int, default=stress_parallel, help=f"So song song (mac dinh: {stress_parallel})")
    parser.add_argument("--models", type=str, nargs="*", default=None, help="Loc model")
    parser.add_argument("--filter", type=str, nargs="*", default=None, help="Loc tu khoa ten scenario (nhieu tu cach nhau bang khoang trang)")
    parser.add_argument("--report", type=str, default=None, help="Path xuat bao cao JSON")
    parser.add_argument("--show-output", action="store_true", help="Hien noi dung output cua model")
    args = parser.parse_args()

    # Tai moi scenario
    basic_oai = load_scenarios("scenarios/basic", "openai", args.filter)
    basic_anth = load_scenarios("scenarios/basic", "anthropic", args.filter)
    repair_sc = load_scenarios("scenarios/repair", None, args.filter)
    all_scenarios = basic_oai + basic_anth + repair_sc

    models = args.models or ["deepseek-default", "deepseek-expert"]

    port = config["port"]
    oai_client = OpenAI(base_url=f"http://127.0.0.1:{port}/v1", api_key=api_key)
    anth_client = Anthropic(
        base_url=f"http://127.0.0.1:{port}/anthropic", api_key=api_key,
        default_headers={"Authorization": f"Bearer {api_key}"},
        http_client=httpx.Client(timeout=120),
    )

    total_scenarios = len(all_scenarios)
    total_requests = total_scenarios * len(models) * args.iterations

    print(f"\nStress test e2e")
    print(f"  Scenario: {total_scenarios} (basic + repair)")
    print(f"  Model: {', '.join(models)}")
    print(f"  Vong lap: {args.iterations} lan/Scenario/Model")
    print(f"  Song song: {args.parallel}")
    print(f"  Tong: {total_requests} request\n")

    tasks: list[tuple[str, str, dict, int]] = []
    for model in models:
        for sc in all_scenarios:
            for i in range(args.iterations):
                tasks.append((sc["endpoint"], model, sc, i))

    all_results: list[dict[str, Any]] = [None] * len(tasks)  # type: ignore[list-item]

    start_total = time.time()
    with ThreadPoolExecutor(max_workers=args.parallel) as executor:
        def run_task(endpoint: str, model: str, sc: dict, _idx: int) -> tuple[int, dict]:
            if endpoint == "openai":
                result = run_openai(oai_client, sc, model)
            else:
                result = run_anthropic(anth_client, sc, model)
            return (_idx, result)

        ep_label = {"openai": "OAI", "anthropic": "ANT"}
        task_labels: dict[int, str] = {}
        for i, (ep, model, sc, it) in enumerate(tasks):
            task_labels[i] = f"{ep_label.get(ep, '?')} | {sc['name']} | {model} | iter-{it + 1}"

        future_map = {}
        for i, (ep, model, sc, _) in enumerate(tasks):
            future = executor.submit(run_task, ep, model, sc, i)
            future_map[future] = i

        done = 0
        passed = 0
        for future in as_completed(future_map):
            idx = future_map[future]
            _, result = future.result()
            all_results[idx] = result
            done += 1
            if result["passed"]:
                passed += 1
            label = task_labels[idx]
            status = "✓" if result["passed"] else "✗"
            err = f" | {result['error'][:60]}" if result["error"] else ""
            print(f"  [{done}/{total_requests}] {status} | {label} | {result['duration']:.1f}s{err}")
            if args.show_output:
                from runner import _print_output
                _print_output(result)

    total_duration = time.time() - start_total
    print(f"\n  Tong thoi gian: {format_duration(total_duration)}")

    report = print_report(all_results, "Bao cao stress test e2e", args.parallel)
    report["total_duration"] = round(total_duration, 1)

    if args.report:
        with open(args.report, "w", encoding="utf-8") as f:
            json.dump({
                "suite": "stress",
                "started_at": datetime.now().strftime("%Y-%m-%d %H:%M:%S"),
                "config": {
                    "iterations": args.iterations,
                    "parallel": args.parallel,
                    "models": models,
                    "accounts": config["accounts"],
                },
                "summary": report,
                "results": all_results,
            }, f, ensure_ascii=False, indent=2)
        print(f"  Bao cao da xuat: {args.report}")

    sys.exit(0 if report["failed"] == 0 else 1)


if __name__ == "__main__":
    main()
