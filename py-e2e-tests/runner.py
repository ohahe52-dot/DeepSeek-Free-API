#!/usr/bin/env python3
"""Diem vao test e2e thong nhat - tai va chay file scenario JSON trong scenarios/

Cach dung:
  uv run python runner.py scenarios/basic                    # tat ca basic (hai endpoint)
  uv run python runner.py scenarios/basic --endpoint openai   # chi OpenAI
  uv run python runner.py scenarios/repair                    # tat ca repair
  uv run python runner.py scenarios/basic --filter stream       # loc theo ten
"""

import argparse
import json
import sys
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from datetime import datetime
from pathlib import Path
from typing import Any

import httpx
from openai import OpenAI
from anthropic import Anthropic

from config import load_config


def load_scenarios(scenario_dir: str, endpoint: str | None, filter_names: list[str] | None) -> list[dict]:
    """Tai file scenario JSON"""
    base = Path(scenario_dir)
    if not base.exists():
        print(f"[Loi] Thu muc scenario khong ton tai: {scenario_dir}")
        sys.exit(1)

    if base.name == "basic":
        dirs = []
        if endpoint in (None, "openai"):
            dirs.append(base / "openai")
        if endpoint in (None, "anthropic"):
            dirs.append(base / "anthropic")
    else:
        dirs = [base]

    scenarios: list[dict] = []
    for d in dirs:
        if not d.exists():
            continue
        for fpath in sorted(d.glob("*.json")):
            with open(fpath) as f:
                sc = json.load(f)
            if filter_names and not any(f.lower() in sc.get("name", "").lower() for f in filter_names):
                continue
            scenarios.append(sc)

    if not scenarios:
        print(f"[Loi] Khong tim thay scenario phu hop")
        sys.exit(1)
    return scenarios


def _resolve_scenario(scenario: dict, model: str) -> dict[str, Any]:
    """Parse dinh nghia scenario thanh tham so OpenAI API"""
    # messages co the o cap cao nhat hoac trong request
    messages = scenario.get("messages") or scenario["request"]["messages"]
    system = scenario.get("system", "")
    if system:
        messages = [{"role": "system", "content": system}, *messages]

    kwargs: dict[str, Any] = dict(model=model, messages=messages)
    # Gop tham so trong request tru stream
    req = scenario.get("request", {})
    kwargs.update({k: v for k, v in req.items() if k != "stream"})

    if "tools" in scenario:
        kwargs["tools"] = scenario["tools"]
    if "tool_choice" in scenario:
        kwargs["tool_choice"] = scenario["tool_choice"]

    return kwargs


def run_openai(client: OpenAI, scenario: dict, model: str) -> dict[str, Any]:
    """Chay mot scenario endpoint OpenAI"""
    name = scenario["name"]
    req_conf = scenario.get("request", {})
    stream = req_conf.get("stream", False)

    start = time.time()
    result: dict[str, Any] = {
        "name": name, "model": model, "endpoint": "openai",
        "passed": False, "duration": 0.0, "error": None,
    }

    try:
        kwargs = _resolve_scenario(scenario, model)

        if stream:
            collected = _openai_stream_collect(client, **kwargs)
            choice = collected["choices"][0]
        else:
            resp = client.chat.completions.create(**kwargs)
            choice = resp.choices[0]

        result["duration"] = time.time() - start
        result["finish_reason"] = choice.finish_reason
        msg = choice.message
        result["content"] = msg.content or ""
        result["tool_calls"] = [
            {"name": tc.function.name, "arguments": tc.function.arguments}
            for tc in (msg.tool_calls or [])
        ]
        result["has_tool_calls"] = len(result["tool_calls"]) > 0

        # Chay checks
        checks = scenario.get("checks", {})
        errors = _check_openai(checks, result)
        if errors:
            result["error"] = "; ".join(errors)
        else:
            result["passed"] = True

    except Exception as e:
        result["duration"] = time.time() - start
        result["error"] = str(e)

    return result


def _openai_stream_collect(client: OpenAI, **kwargs: Any) -> dict:
    """Request stream: thu moi chunk va lap thanh quasi-Response dict"""
    kwargs["stream"] = True
    stream = client.chat.completions.create(**kwargs)

    content_parts: list[str] = []
    tool_call_acc: dict[int, dict] = {}
    finish_reason: str | None = None

    for chunk in stream:
        if not chunk.choices:
            continue
        choice = chunk.choices[0]
        if choice.finish_reason:
            finish_reason = choice.finish_reason
        if choice.delta.content:
            content_parts.append(choice.delta.content)
        if choice.delta.tool_calls:
            for tc in choice.delta.tool_calls:
                idx = tc.index
                if idx not in tool_call_acc:
                    tool_call_acc[idx] = {
                        "id": tc.id or "",
                        "function": {"name": "", "arguments": ""},
                    }
                if tc.id:
                    tool_call_acc[idx]["id"] = tc.id
                if tc.function:
                    if tc.function.name:
                        tool_call_acc[idx]["function"]["name"] += tc.function.name
                    if tc.function.arguments:
                        tool_call_acc[idx]["function"]["arguments"] += tc.function.arguments

    tool_calls_list = sorted(tool_call_acc.values(), key=lambda x: list(tool_call_acc.keys())[list(tool_call_acc.values()).index(x)])
    class FakeChoice:
        def __init__(self, finish: str | None, content: str | None, tcs: list):
            self.finish_reason = finish
            self.message = type("Msg", (), {
                "content": content,
                "tool_calls": [type("TC", (), {"function": type("Fn", (), tc["function"])}) for tc in tcs] if tcs else None,
            })()
    return {"choices": [FakeChoice(finish_reason, "".join(content_parts) or None, tool_calls_list)]}


def _check_openai(checks: dict, result: dict) -> list[str]:
    errors: list[str] = []
    if checks.get("content_not_empty") and not result.get("content"):
        errors.append("Noi dung rong")
    if checks.get("has_tool_calls") and not result.get("has_tool_calls"):
        errors.append("Chua kich hoat goi cong cu")
    if checks.get("finish_reason") and result.get("finish_reason") != checks["finish_reason"]:
        errors.append(f"finish_reason={result.get('finish_reason')}, ky vong={checks['finish_reason']}")
    if checks.get("tool_names"):
        actual = {tc["name"] for tc in result.get("tool_calls", [])}
        expected = set(checks["tool_names"])
        if not expected.issubset(actual):
            errors.append(f"Ten cong cu khong khop: ky vong{expected}, thuc te{actual}")
    return errors


def run_anthropic(client: Anthropic, scenario: dict, model: str) -> dict[str, Any]:
    """Chay mot scenario endpoint Anthropic"""
    name = scenario["name"]
    req_conf = scenario.get("request", {})

    start = time.time()
    result: dict[str, Any] = {
        "name": name, "model": model, "endpoint": "anthropic",
        "passed": False, "duration": 0.0, "error": None,
    }

    try:
        # messages cua Anthropic luon nam trong request
        kwargs: dict[str, Any] = dict(
            model=model,
            **{k: v for k, v in req_conf.items() if k != "stream"},
        )
        stream = req_conf.get("stream", False)

        if stream:
            msg = _anthropic_stream_collect(client, **kwargs)
        else:
            msg = client.messages.create(**kwargs)

        result["duration"] = time.time() - start
        result["stop_reason"] = msg.stop_reason

        text_blocks = []
        tool_uses = []
        for block in msg.content:
            if block.type == "text":
                text_blocks.append(block.text)
            elif block.type == "tool_use":
                tool_uses.append({"name": block.name, "input": block.input})

        result["content"] = "".join(text_blocks)
        result["tool_uses"] = tool_uses
        result["has_tool_use"] = len(tool_uses) > 0

        checks = scenario.get("checks", {})
        errors = _check_anthropic(checks, result)
        if errors:
            result["error"] = "; ".join(errors)
        else:
            result["passed"] = True

    except Exception as e:
        result["duration"] = time.time() - start
        result["error"] = str(e)

    return result


def _anthropic_stream_collect(client: Anthropic, **kwargs: Any) -> Any:
    """Request stream: thu event stream Anthropic"""
    kwargs = {k: v for k, v in kwargs.items() if v is not None}
    with client.messages.stream(**kwargs) as stream:
        return stream.get_final_message()


def _check_anthropic(checks: dict, result: dict) -> list[str]:
    errors: list[str] = []
    if checks.get("content_not_empty") and not result.get("content"):
        errors.append("Noi dung rong")
    if checks.get("has_tool_use") and not result.get("has_tool_use"):
        errors.append("Chua kich hoat goi cong cu")
    if checks.get("stop_reason") and result.get("stop_reason") != checks["stop_reason"]:
        errors.append(f"stop_reason={result.get('stop_reason')}, ky vong={checks['stop_reason']}")
    if checks.get("tool_names"):
        actual = {tu["name"] for tu in result.get("tool_uses", [])}
        expected = set(checks["tool_names"])
        if not expected.issubset(actual):
            errors.append(f"Ten cong cu khong khop: ky vong{expected}, thuc te{actual}")
    return errors


def _print_output(result: dict) -> None:
    """In noi dung output cua model (dung cho --show-output)"""
    content = (result.get("content") or "")[:300].replace("\n", "\\n")
    if content:
        print(f"    ├ Tra loi: {content}")
    if result.get("has_tool_calls") or result.get("has_tool_use"):
        calls = result.get("tool_calls") or result.get("tool_uses") or []
        for tc in calls:
            name = tc.get("name", "?")
            args = tc.get("arguments") or tc.get("input") or {}
            args_str = json.dumps(args, ensure_ascii=False)[:120]
            print(f"    ├ Cong cu: {name}({args_str})")
    fr = result.get("finish_reason") or result.get("stop_reason") or ""
    if fr:
        print(f"    └ Ket thuc: {fr}")
    if result.get("error"):
        print(f"    └ Loi: {result['error']}")


def format_duration(seconds: float) -> str:
    if seconds < 60:
        return f"{seconds:.1f}s"
    return f"{seconds / 60:.1f}m"


def print_report(results: list[dict[str, Any]], suite_name: str, parallel: int):
    total = len(results)
    passed = sum(1 for r in results if r["passed"])
    duration = sum(r["duration"] for r in results)

    print(f"\n{'=' * 60}")
    print(f"  {suite_name}")
    print(f"  Thoi gian: {datetime.now().strftime('%Y-%m-%d %H:%M:%S')}")
    print(f"  Song song: {parallel}")
    print(f"{'=' * 60}")
    print(f"  Tong: {total}  |  Dat: {passed}  |  That bai: {total - passed}  |  Tong thoi gian: {format_duration(duration)}")

    ep_label = {"openai": "OAI", "anthropic": "ANT"}
    for r in sorted(results, key=lambda x: (x["name"], x.get("endpoint", ""), x["model"])):
        status = "✓" if r["passed"] else "✗"
        ep = ep_label.get(r.get("endpoint", ""), "?")
        err = f" | {r['error'][:60]}" if r["error"] else ""
        print(f"    {status} {ep} | {r['name']} | {r['model']} | {r['duration']:6.1f}s{err}")

    if total - passed > 0:
        print(f"\n  {'─' * 48}")
        print(f"  Chi tiet that bai:")
        for r in results:
            if not r["passed"]:
                print(f"  [{r['endpoint']}] {r['name']} ({r['model']}): {r['error']}")

    print(f"{'=' * 60}\n")
    return {"total": total, "passed": passed, "failed": total - passed, "duration": duration}


def main():
    config = load_config()
    safe_concurrency = config["safe_concurrency"]
    api_key = config["api_key"]

    parser = argparse.ArgumentParser(description="Diem vao test e2e thong nhat")
    parser.add_argument("scenario_dir", help="Thu muc scenario (vi du scenarios/basic hoac scenarios/repair)")
    parser.add_argument("--endpoint", choices=["openai", "anthropic"], default=None, help="Loc endpoint")
    parser.add_argument("--model", type=str, default=None, help="Loc model")
    parser.add_argument("--filter", type=str, nargs="*", default=None, help="Loc tu khoa ten scenario (nhieu tu cach nhau bang khoang trang)")
    parser.add_argument("--parallel", type=int, default=safe_concurrency, help=f"So song song (mac dinh: {safe_concurrency})")
    parser.add_argument("--report", type=str, default=None, help="Path xuat bao cao JSON")
    parser.add_argument("--show-output", action="store_true", help="Hien noi dung output cua model")
    args = parser.parse_args()

    scenarios = load_scenarios(args.scenario_dir, args.endpoint, args.filter)
    # Nguon model: uu tien tham so --model, neu khong lay dong tu config.toml
    models = [args.model] if args.model else config.get("models", ["deepseek-default"])

    port = config["port"]
    oai_client = OpenAI(base_url=f"http://127.0.0.1:{port}/v1", api_key=api_key)
    anth_client = Anthropic(
        base_url=f"http://127.0.0.1:{port}/anthropic", api_key=api_key,
        default_headers={"Authorization": f"Bearer {api_key}"},
        http_client=httpx.Client(timeout=120),
    )

    suite_name = f"{Path(args.scenario_dir).name} test"
    print(f"\n{suite_name}")
    print(f"  Scenario: {len(scenarios)} , Model: {', '.join(models)}, Song song: {args.parallel}")

    tasks: list[tuple[str, str, dict]] = []
    for model in models:
        for sc in scenarios:
            tasks.append((sc["endpoint"], model, sc))

    all_results: list[dict[str, Any]] = [None] * len(tasks)  # type: ignore[list-item]

    # Ghi label moi task de hien tien do
    ep_label = {"openai": "OAI", "anthropic": "ANT"}
    task_labels: dict[int, str] = {}
    for i, (ep, model, sc) in enumerate(tasks):
        task_labels[i] = f"{ep_label.get(ep, '?')} | {sc['name']} | {model}"

    with ThreadPoolExecutor(max_workers=args.parallel) as executor:
        def run_task(endpoint: str, model: str, sc: dict) -> tuple[int, dict]:
            if endpoint == "openai":
                return (0, run_openai(oai_client, sc, model))
            return (0, run_anthropic(anth_client, sc, model))

        future_map = {}
        for i, (ep, model, sc) in enumerate(tasks):
            future = executor.submit(run_task, ep, model, sc)
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
            print(f"  [{done}/{len(tasks)}] {status} | {label} | {result['duration']:.1f}s{err}")
            if args.show_output:
                _print_output(result)

    report = print_report(all_results, suite_name, args.parallel)

    if args.report:
        with open(args.report, "w", encoding="utf-8") as f:
            json.dump({
                "suite": suite_name,
                "started_at": datetime.now().strftime("%Y-%m-%d %H:%M:%S"),
                "config": {"parallel": args.parallel, "accounts": config["accounts"]},
                "summary": report,
                "results": all_results,
            }, f, ensure_ascii=False, indent=2)
        print(f"  Bao cao da xuat: {args.report}")

    sys.exit(0 if report["failed"] == 0 else 1)


if __name__ == "__main__":
    main()
