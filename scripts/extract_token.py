#!/usr/bin/env python3
import sys
import time
import json
from pathlib import Path

from playwright.sync_api import sync_playwright

PROFILE_DIR = Path.home() / ".deepseek-agent" / "browser"
PROFILE_DIR.mkdir(parents=True, exist_ok=True)


def try_extract(headless: bool, wait_secs: int):
    with sync_playwright() as p:
        ctx = p.chromium.launch_persistent_context(
            user_data_dir=str(PROFILE_DIR),
            headless=headless,
            args=["--disable-blink-features=AutomationControlled"],
        )
        page = ctx.pages[0] if ctx.pages else ctx.new_page()
        page.goto("https://chat.deepseek.com", wait_until="domcontentloaded")

        deadline = time.time() + wait_secs
        token = None
        while time.time() < deadline:
            try:
                raw = page.evaluate("() => localStorage.getItem('userToken')")
                if raw:
                    t = json.loads(raw).get("value", "")
                    if t:
                        token = t
                        break
            except Exception:
                pass
            time.sleep(1)
        ctx.close()
        return token


def main():
    token = try_extract(headless=True, wait_secs=8)
    if token:
        print(token, flush=True)
        return

    print("[token] 需要登录，正在打开浏览器...", file=sys.stderr)
    token = try_extract(headless=False, wait_secs=180)
    if token:
        print(token, flush=True)
        return

    print("ERROR: 等待登录超时", file=sys.stderr)
    sys.exit(1)


if __name__ == "__main__":
    main()