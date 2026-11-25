"""Drive the local diagnostic page against a native WebRTC probe in CI."""

import json
import os
from pathlib import Path
import subprocess
import sys
import time
from urllib.error import URLError
from urllib.request import Request, urlopen


DRIVER_URL = "http://127.0.0.1:9515"


def webdriver(method, path, body=None):
    data = None if body is None else json.dumps(body).encode("utf-8")
    request = Request(
        DRIVER_URL + path,
        data=data,
        method=method,
        headers={"Content-Type": "application/json"},
    )
    with urlopen(request, timeout=5) as response:
        result = json.load(response)
    if isinstance(result.get("value"), dict) and result["value"].get("error"):
        raise RuntimeError(result["value"])
    return result


def execute(session, script, *args):
    return webdriver(
        "POST", f"/session/{session}/execute/sync", {"script": script, "args": args}
    )["value"]


def main():
    if len(sys.argv) != 3:
        raise SystemExit("usage: browser_probe.py PAGE_URL ROOM_HEX")
    driver = Path(os.environ["CHROMEWEBDRIVER"]) / "chromedriver"
    process = subprocess.Popen(
        [str(driver), "--port=9515"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.STDOUT,
    )
    session = None
    try:
        for _ in range(40):
            try:
                webdriver("GET", "/status")
                break
            except URLError:
                time.sleep(0.25)
        else:
            raise RuntimeError("ChromeDriver did not start")
        created = webdriver("POST", "/session", {
            "capabilities": {"alwaysMatch": {
                "browserName": "chrome",
                "goog:chromeOptions": {"args": [
                    "--headless=new", "--no-sandbox", "--disable-dev-shm-usage"
                ]},
            }}
        })
        session = created["value"]["sessionId"]
        webdriver("POST", f"/session/{session}/url", {"url": sys.argv[1]})
        for _ in range(40):
            if execute(session, "return typeof document.querySelector('#start')?.onclick === 'function'"):
                break
            time.sleep(0.25)
        else:
            raise RuntimeError("diagnostic page did not initialize")
        execute(session, "document.querySelector('#room').value = arguments[0]; document.querySelector('#start').click()", sys.argv[2])
        deadline = time.monotonic() + 35
        while time.monotonic() < deadline:
            log = execute(session, "return document.querySelector('#log').textContent")
            if "応答 " in log and "DataChannel接続" in log:
                print(log)
                return
            time.sleep(0.5)
        print(log)
        raise RuntimeError("browser did not exchange a DataChannel ping")
    finally:
        if session:
            try:
                webdriver("DELETE", f"/session/{session}")
            except Exception:
                pass
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()


if __name__ == "__main__":
    main()
