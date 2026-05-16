#!/usr/bin/env python3
"""Test fallback ngu canh dai - kiem tra phat hien oversized + logic chia chunk

Tao van ban dai vuot nguong de test hai duong fallback: expert (completion chia chunk) va default (upload file).

Cach dung:
  uv run python test_oversized.py
  uv run python test_oversized.py --model deepseek-expert   # chi test expert
  uv run python test_oversized.py --show-output
  uv run python test_oversized.py --model deepseek-expert --show-output
"""

import argparse
import json
import sys
import time
from datetime import datetime
from pathlib import Path

from openai import OpenAI

from config import load_config


def make_long_prompt(target_chars: int) -> str:
    """Tao prompt van ban dai vua vuot threshold
    """
    base = "deepseek"
    repeat = target_chars // len(base) + 1
    return base * repeat


def run_oversized(client: OpenAI, model: str, threshold: int) -> dict:
    """Chay mot test oversized"""
    prompt = make_long_prompt(threshold + 1)

    start = time.time()
    result: dict = {
        "model": model,
        "threshold": threshold,
        "passed": False,
        "duration": 0.0,
        "output_len": 0,
        "output_preview": "",
        "error": None,
    }

    try:
        response = client.chat.completions.create(
            model=model,
            messages=[
                {"role": "system", "content": "Ban la mot tro ly huu ich. Trong moi truong hop, chi tra loi mot cau `Hello, world!`"},
                {"role": "user", "content": prompt},
            ],
            stream=True,
            max_tokens=100,
        )

        content_parts: list[str] = []
        for chunk in response:
            if chunk.choices and chunk.choices[0].delta.content:
                content_parts.append(chunk.choices[0].delta.content)

        result["duration"] = time.time() - start
        result["output_len"] = len("".join(content_parts))
        result["output_preview"] = "".join(content_parts)[:200]
        result["passed"] = len(content_parts) > 0
        if not result["passed"]:
            result["error"] = "Noi dung tra ve rong"

    except Exception as e:
        result["duration"] = time.time() - start
        result["error"] = str(e)

    return result


def _print_output(result: dict) -> None:
    """In output cua model (doi xung voi runner.py)"""
    content = (result.get("output_preview") or "")[:300].replace("\n", "\\n")
    if content:
        print(f"    ├ Tra loi: {content}")
    if result.get("error"):
        print(f"    └ Loi: {result['error']}")


def format_duration(seconds: float) -> str:
    if seconds < 60:
        return f"{seconds:.1f}s"
    return f"{seconds / 60:.1f}m"


def print_report(results: list[dict]) -> dict:
    """In bao cao tong hop (doi xung voi runner.py)"""
    total = len(results)
    passed = sum(1 for r in results if r["passed"])
    duration = sum(r["duration"] for r in results)

    print(f"\n{'=' * 60}")
    print(f"  Test fallback ngu canh dai")
    print(f"  Thoi gian: {datetime.now().strftime('%Y-%m-%d %H:%M:%S')}")
    print(f"{'=' * 60}")
    print(f"  Tong: {total}  |  Dat: {passed}  |  That bai: {total - passed}  |  Tong thoi gian: {format_duration(duration)}")

    for r in sorted(results, key=lambda x: x["model"]):
        status = "✓" if r["passed"] else "✗"
        fallback = "completion chia chunk" if "expert" in r["model"] else "upload file"
        err = f" | {r['error'][:60]}" if r["error"] else ""
        print(f"    {status} {r['model']} | {fallback} | threshold={r['threshold']} | {r['duration']:6.2f}s | {r['output_len']} chars{err}")

    if total - passed > 0:
        print(f"\n  {'─' * 48}")
        print(f"  Chi tiet that bai:")
        for r in results:
            if not r["passed"]:
                print(f"  {r['model']}: {r['error']}")

    print(f"{'=' * 60}\n")
    return {"total": total, "passed": passed, "failed": total - passed, "duration": duration}


def main():
    parser = argparse.ArgumentParser(description="Test fallback ngu canh dai")
    parser.add_argument("--model", type=str, default=None, help="Chi test model chi dinh, vi du deepseek-expert")
    parser.add_argument("--show-output", action="store_true", help="Hien noi dung output cua model")
    parser.add_argument("--report", type=str, default=None, help="Path xuat bao cao JSON")
    args = parser.parse_args()

    config = load_config()
    client = OpenAI(
        base_url=f"http://127.0.0.1:{config['port']}/v1",
        api_key=config["api_key"],
    )

    # Tao bang threshold dong tu config
    threshold_map = {
        f"deepseek-{t}": (limit * 75 // 100)
        for t, limit in zip(config["model_types"], config["input_character_limits"])
    }

    models = [args.model] if args.model else config["models"]

    suite_name = "Test fallback ngu canh dai"
    print(f"\n{suite_name}")
    print(f"  Model: {', '.join(models)}")

    # Test expert truoc (chia chunk), roi default/vision (upload file)
    sorted_models = sorted(models, key=lambda m: (0 if "expert" in m else 1, m))
    fallback_labels = {"expert": "completion chia chunk", "default": "upload file", "vision": "upload file"}

    results: list[dict] = []
    done = 0
    for model in sorted_models:
        threshold = threshold_map.get(model, 122_880)
        fb = next((v for k, v in fallback_labels.items() if k in model), "?")

        r = run_oversized(client, model, threshold)
        results.append(r)
        done += 1

        status = "✓" if r["passed"] else "✗"
        err = f" | {r['error'][:60]}" if r["error"] else ""
        print(f"  [{done}/{len(sorted_models)}] {status} | {fb} | {model} | {r['duration']:.1f}s | {r['output_len']} chars{err}")
        if args.show_output:
            _print_output(r)

    report = print_report(results)

    if args.report:
        with open(args.report, "w", encoding="utf-8") as f:
            json.dump({
                "suite": suite_name,
                "started_at": datetime.now().strftime("%Y-%m-%d %H:%M:%S"),
                "summary": report,
                "results": results,
            }, f, ensure_ascii=False, indent=2)
        print(f"  Bao cao da xuat: {args.report}")

    sys.exit(0 if report["failed"] == 0 else 1)


if __name__ == "__main__":
    main()
