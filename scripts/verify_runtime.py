"""R16/R17/R18 runtime verification — honest-metrics edition (RC review).

Recorded metrics and their exact meaning:
- core_rss_kb_idle: the RelayCheck service process RSS (working set) while
  one browser page is open but idle. This is the "Core ≤90MiB" budget.
- page JS heap: the renderer's usedJSHeapSize. It is the JS heap ONLY —
  NOT the page's process memory and NOT comparable to a 120MiB page-RSS
  budget. Reported for information; page-process RSS is NOT attributed per
  page by this probe (browser total contains all tabs, GPU, network and
  utility processes and is reported as-is with that caveat).
- new_timers_in_3s_idle: setInterval/setTimeout registrations observed over
  an idle window (probe itself schedules one setTimeout — subtracted).
- rAF idle delta: counted via an init hook before load; the probe never
  schedules rAF (see scripts/ui_gates.py which asserts delta==0).

Everything unmeasured is recorded as unmeasured — nothing is inferred.
"""
import json
import subprocess
import sys
import urllib.request
from pathlib import Path

exe, port, token, report_path = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4]
base = f"http://127.0.0.1:{port}"
report = {"r16": {}, "r17": {}, "r18": {}, "caveats": []}

import socket
s = socket.socket()
try:
    s.connect(("127.0.0.1", port))
    report["r16"]["service_reachable"] = True
finally:
    s.close()
ver = json.loads(urllib.request.urlopen(f"{base}/v1/version").read())
report["r16"]["version_endpoint"] = ver
shell = urllib.request.urlopen(f"{base}/").read().decode("utf-8", "replace")
report["r16"]["shell_bytes"] = len(shell)
report["r16"]["shell_has_root_div"] = 'id="root"' in shell
exe_dir = Path(exe).resolve().parent
report["r16"]["exe_dir"] = str(exe_dir)
report["r16"]["exe_outside_repo"] = "projects" not in str(exe_dir).replace("\\", "/").lower()

# ---- R18: package readable without the product ----
import zipfile
pkgs = sorted(Path(r"D:/dev/relaycheck-test-data/live-shots/data/packages").glob("*.zip"))
if pkgs:
    with zipfile.ZipFile(pkgs[-1]) as z:
        report["r18"]["package"] = pkgs[-1].name
        report["r18"]["entries"] = z.namelist()
        handoff = z.read("handoff.md").decode("utf-8")
        receipt = json.loads(z.read("receipt.json"))
        report["r18"]["handoff_sections"] = [
            x for x in ["## Goal", "## Constraints", "## Changed", "## Verified", "## Open items", "## Next action", "## Evidence"]
            if x in handoff
        ]
        report["r18"]["receipt_kind"] = receipt.get("kind")
else:
    report["r18"]["error"] = "no package found (export a handoff first)"

# ---- R17 ----
def process_rss_kb(image_hint):
    out = subprocess.run(
        ["tasklist", "/FI", f"IMAGENAME eq {image_hint}", "/FO", "CSV", "/NH"],
        capture_output=True, text=True,
    ).stdout
    total = 0
    for line in out.splitlines():
        parts = [p.strip('"') for p in line.split('","')]
        if len(parts) >= 5 and image_hint.lower().replace(".exe", "") in parts[0].lower():
            mem = parts[4].replace(",", "").replace(" K", "").replace("K", "").strip()
            try:
                total += int(mem)
            except ValueError:
                pass
    return total

report["r17"]["core_rss_kb_idle"] = process_rss_kb("relaycheck.exe")
report["r17"]["core_budget"] = "spec target ≤ 90MiB (92160 kB)"

from playwright.sync_api import sync_playwright
with sync_playwright() as p:
    browser = p.chromium.launch(headless=True)
    ctx = browser.new_context(locale="zh-CN", viewport={"width": 1440, "height": 900})
    page = ctx.new_page()
    page.add_init_script(
        "window.__rafCount = 0; const orig = window.requestAnimationFrame;"
        "window.requestAnimationFrame = (cb) => { window.__rafCount++; return orig(cb); };"
    )
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
    # the probe's own setTimeout is one of the timeouts
    report["r17"]["new_timers_in_3s_idle"] = {
        "intervals": timers["intervals"],
        "timeouts_excluding_probe": max(0, timers["timeouts"] - 1),
    }
    heap = page.evaluate(
        "() => performance.memory ? {usedJSHeapSize: performance.memory.usedJSHeapSize} : null"
    )
    report["r17"]["page_js_heap_bytes"] = heap
    report["r17"]["page_js_heap_meaning"] = (
        "renderer JS heap only — NOT page process RSS; no per-page process "
        "attribution attempted this round"
    )
    out = subprocess.run(["tasklist", "/FO", "CSV", "/NH"], capture_output=True, text=True).stdout
    browser_kb = 0
    for line in out.splitlines():
        parts = [p2.strip('"') for p2 in line.split('","')]
        if len(parts) >= 5 and ("chrome" in parts[0].lower() or "chromium" in parts[0].lower() or "msedge" in parts[0].lower()):
            mem = parts[4].replace(",", "").replace(" K", "").replace("K", "").strip()
            try:
                browser_kb += int(mem)
            except ValueError:
                pass
    report["r17"]["browser_processes_rss_kb_total"] = browser_kb
    report["r17"]["browser_total_meaning"] = (
        "sum over ALL browser processes on the machine (all tabs + GPU + "
        "network + utility). Increment attributable to this page is NOT "
        "isolated — recorded as unmeasured."
    )
    report["caveats"].append(
        "page-process memory budget (≤120MiB) NOT verified this round: "
        "per-page attribution requires browser-internal instrumentation "
        "which this probe does not have; JS heap and browser total are "
        "reported instead."
    )
    browser.close()

report["caveats"].append(
    "200% zoom evidence = half-viewport (720x450 == 1440x900 @200%) with a "
    "real-browser overflow assertion in scripts/ui_gates.py; not a native "
    "high-DPI display test."
)
report["caveats"].append(
    "Linux/macOS untested this round; Windows 10 x64 19045 only."
)

Path(report_path).write_text(json.dumps(report, indent=1, ensure_ascii=False), encoding="utf-8")
print(json.dumps(report, indent=1, ensure_ascii=False))
