#!/usr/bin/env python3
"""Module cau hinh chung e2e - tai py-e2e-tests/config.toml"""

import sys
from pathlib import Path

import tomllib


def load_config() -> dict:
    config_path = Path(__file__).parent / "config.toml"
    if not config_path.exists():
        print(f"[Loi] Khong tim thay {config_path}")
        print(f"  Hay copy va sua tu thu muc goc project:")
        print(f"    cp config.example.toml {config_path}")
        print(f"  Sau do doi [[accounts]] thanh accounts = [] (e2e test khong can tai khoan that)")
        sys.exit(1)

    with open(config_path, "rb") as f:
        config = tomllib.load(f)

    ds = config.get("deepseek", {})
    model_types = ds.get("model_types", ["default", "expert", "vision"])
    models = [f"deepseek-{t}" for t in model_types]

    api_keys = config.get("api_keys", [])
    api_key = api_keys[0]["key"] if api_keys else "sk-test"
    port = config.get("server", {}).get("port", 22217)
    accounts = len(config.get("accounts", []))

    return {
        "api_key": api_key,
        "port": port,
        "models": models,
        "model_types": model_types,
        "input_character_limits": ds.get("input_character_limits", [2_621_440, 163_840, 2_621_440]),
        "accounts": accounts,
        "safe_concurrency": max(1, accounts // 2),
    }
