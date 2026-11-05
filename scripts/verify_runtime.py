"""R16/R17/R18 runtime verification.

- R16: run the candidate from OUTSIDE the source tree; static assets,
  NOTICE/version endpoints serve; gitleaks-style fixture hygiene is checked
  separately by the repo scan.
- R17: idle resource sampling — Core RSS, browser page memory (JS heap),
  and proof there is no periodic polling/timer churn when idle.
- R18: the exported package is readable WITHOUT RelayCheck (pure zipfile).

Writes a JSON report to the path given as the last argument.
"""
import json
import subprocess
import sys
import time
import zipfile
from pathlib import Path

from playwright.sync_api import sync_playwright

exe, port, token, report_path = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4]
base = f"http://127.0.0.1:{port}"
report = {"r16": {}, "r17": {}, "r18": {}}

# ---- R16: exe runs from a source-external directory ----
import socket
s = socket.socket()
try:
    s.connect(("127.0.0.1", port))
    report["r16"]["service_reachable"] = True
finally:
    s.close()
import urllib.request
ver = json.loads(urllib.request.urlopen(f"{base}/v1/version").read())
report["r16"]["version_endpoint"] = ver
shell = urllib.request.urlopen(f"{base}/").read().decode("utf-8", "replace")
report["r16"]["shell_bytes"] = len(shell)
report["r16"]["shell_has_root_div"] = 'id="root"' in shell
exe_dir = Path(exe).resolve().parent
report["r16"]["exe_dir"] = str(exe_dir)
report["r16"]["exe_outside_repo"] = "projects" not in str(exe_dir).replace("\\", "/").lower()

# ---- R18: package readable without the product ----
pkgs = sorted(Path(r"D:/dev/relaycheck-test-data/live-shots/data/packages").glob("*.zip"))
if pkgs:
    with zipfile.ZipFile(pkgs[-1]) as z:
        names = z.namelist()
        report["r18"]["package"] = pkgs[-1].name
        report["r18"]["entries"] = names
        handoff = z.read("handoff.md").decode("utf-8")
        receipt = json.loads(z.read("receipt.json"))
        report["r18"]["handoff_sections"] = [
            x for x in ["## Goal", "## Constraints", "## Changed", "## Verified", "## Open items", "## Next action", "## Evidence"]
            if x in handoff
        ]
        report["r18"]["receipt_kind"] = receipt.get("kind")
else:
    report["r18"]["error"] = "no package found (export a handoff first)"

# ---- R17: idle resources ----
def core_rss_kb(name_hint="relaycheck"):
    out = subprocess.run(
        ["tasklist", "/FI", f"IMAGENAME eq {name_hint}.exe", "/FO", "CSV", "/NH"],
        capture_output=True, text=True,
    ).stdout
    total = 0
    for line in out.splitlines():
        parts = [p.strip('"') for p in line.split('","')]
        if len(parts) >= 5 and name_hint in parts[0].lower():
            mem = parts[4].replace(",", "").replace(" K", "").replace("K", "").strip()
            try:
                total += int(mem)
            except ValueError:
                pass
    return total

report["r17"]["core_rss_kb_idle"] = core_rss_kb()

with sync_playwright() as p:
    browser = p.chromium.launch(headless=True)
    ctx = browser.new_context(locale="zh-CN", viewport={"width": 1440, "height": 900})
    page = ctx.new_page()
    page.goto(f"{base}/#rcb={token}")
    page.wait_for_load_state("domcontentloaded")
    page.wait_for_timeout(1500)

    timers = page.evaluate(
        """() => new Promise(res => {
        let intervals = 0, timeouts = 0;
        const oi = window.setInterval, ot = window.setTimeout;
        window.setInterval = (...a) => { intervals++; return oi(...a); };
        window.setTimeout = (...a) => { timeouts++; return ot(...a); };
        setTimeout(() => res({intervals, timeouts}), 3000);
    })"""
    )
    report["r17"]["new_timers_in_3s_idle"] = timers
    heap = page.evaluate("() => performance.memory ? {usedJSHeapSize: performance.memory.usedJSHeapSize, jsHeapSizeLimit: performance.memory.jsHeapSizeLimit} : null")
    report["r17"]["page_js_heap"] = heap
    # browser processes RSS (all chromium children)
    out = subprocess.run(["tasklist", "/FO", "CSV", "/NH"], capture_output=True, text=True).stdout
    chrome_kb = 0
    for line in out.splitlines():
        parts = [p2.strip('"') for p2 in line.split('","')]
        if len(parts) >= 5 and ("chrome" in parts[0].lower() or "chromium" in parts[0].lower() or "msedge" in parts[0].lower()):
            mem = parts[4].replace(",", "").replace(" K", "").replace("K", "").strip()
            try:
                chrome_kb += int(mem)
            except ValueError:
                pass
    report["r17"]["browser_processes_rss_kb_total"] = chrome_kb
    browser.close()

Path(report_path).write_text(json.dumps(report, indent=1, ensure_ascii=False), encoding="utf-8")
print(json.dumps(report, indent=1, ensure_ascii=False))
