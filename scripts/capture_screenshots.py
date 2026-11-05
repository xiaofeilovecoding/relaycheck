"""Production screenshot capture for the visual gates (V01–V06, R15).

Runs the REAL production binary (built by the caller), walks the real UI:
demo task → checks → handoff → return B → accept, in both languages and
themes, at 1440/390/320 widths, 200% zoom, with keyboard and reduced-motion
probes. Screenshots land in artifacts/relaycheck/RELAYCHECK-001/screenshots/.

Usage:
  python scripts/capture_screenshots.py <exe-path> <port> <bootstrap-token> <out-dir>
"""
import json
import os
import sys
import time
from pathlib import Path

from playwright.sync_api import sync_playwright

SET_V = """(el, v) => {
  const proto = Object.getPrototypeOf(el);
  Object.getOwnPropertyDescriptor(proto, 'value').set.call(el, v);
  el.dispatchEvent(new Event('input', { bubbles: true }));
}"""


def wait_page(page, ms=1200):
    page.wait_for_timeout(ms)


def click_text(page, selector, text, retries=6, delay=800):
    for attempt in range(retries):
        try:
            page.evaluate(
                """([sel, t]) => {
                const els = [...document.querySelectorAll(sel)].filter(b => b.textContent.includes(t));
                if (!els.length) throw new Error('no element: ' + t);
                els[els.length - 1].click();
                return els.length;
            }""",
                [selector, text],
            )
            return
        except Exception:
            if attempt == retries - 1:
                body = page.evaluate(
                    "() => (document.getElementById('root')?.textContent ?? '').replace(/\\s+/g, ' ').slice(0, 500)"
                )
                st = page.evaluate(
                    """async () => {
                    const r = await fetch('/v1/state').then(x => x.json());
                    return JSON.stringify(r.tasks && r.tasks.map(t => ({g: t.goal.slice(0,18), s: t.workflow_state})));
                }"""
                )
                raise RuntimeError(f"click '{text}' failed; page: {body} | state: {st}")
            page.wait_for_timeout(delay)


def run(exe, port, token, out_dir):
    out = Path(out_dir)
    out.mkdir(parents=True, exist_ok=True)
    base = f"http://127.0.0.1:{port}"
    shots = []

    def snap(page, name):
        path = out / f"{name}.png"
        page.screenshot(path=str(path), full_page=False)
        shots.append(str(path))
        print("captured", path)

    with sync_playwright() as p:
        browser = p.chromium.launch(headless=True)
        ctx = browser.new_context(locale="zh-CN", viewport={"width": 1440, "height": 900})
        page = ctx.new_page()
        errors = []
        page.on("pageerror", lambda e: errors.append(str(e)))
        # No external requests beyond self — collect any non-self requests.
        external = []
        page.on("request", lambda r: external.append(r.url) if f"127.0.0.1:{port}" not in r.url else None)

        page.goto(f"{base}/#rcb={token}")
        page.wait_for_load_state("domcontentloaded")
        wait_page(page, 1500)

        # Demo journey (real local repo + real check).
        click_text(page, "main button", "试用样例")
        wait_page(page, 4500)
        click_text(page, ".task-card", "重试")
        wait_page(page, 1800)
        # Run checks.
        click_text(page, "main button", "运行本机检查")
        wait_page(page, 5000)
        snap(page, "desktop-light-zh-ready")

        # Handoff export (V05 source side).
        click_text(page, "main button", "预览并导出接手卡")
        wait_page(page, 1000)
        page.evaluate(
            """() => {
            const set = (el, v) => {
              const proto = Object.getPrototypeOf(el);
              Object.getOwnPropertyDescriptor(proto, 'value').set.call(el, v);
              el.dispatchEvent(new Event('input', { bubbles: true }));
            };
            const tas = [...document.querySelectorAll('.modal textarea')];
            set(tas[0], '先阅读 tests/test_retry.py，复现等待重试时的取消行为。');
            set(tas[1], 'C2: 取消行为尚未验证');
        }""",
            None,
        )
        wait_page(page, 400)
        click_text(page, ".modal button", "导出接手包")
        wait_page(page, 2500)
        snap(page, "handoff-light-zh-preview")
        page.keyboard.press("Escape")
        wait_page(page, 600)

        # Return B: external tool edits the demo repo, capture through the UI.
        import glob
        repos = glob.glob(r"D:/dev/relaycheck-test-data/live-shots/data/demo/repo-*")
        assert repos, "demo repo not found"
        repo = Path(repos[-1])
        (repo / "src" / "retry.py").write_text(
            "def retry(fn, times=3, cancelled=None):\n    if cancelled:\n        raise TimeoutError()\n    return fn()\n",
            encoding="utf-8",
        )
        (repo / "tests" / "test_retry.py").write_text(
            "def test_retry():\n    assert retry(lambda: 1) == 1\n\ndef test_cancel():\n    assert True\n",
            encoding="utf-8",
        )
        click_text(page, "main button", "捕获候选")
        wait_page(page, 1500)
        page.evaluate("() => { document.querySelector('.modal input[type=checkbox]').click(); }")
        wait_page(page, 300)
        click_text(page, ".modal button", "确认捕获")
        wait_page(page, 5000)
        snap(page, "desktop-light-zh-returned")

        # Checks on B, then accept.
        click_text(page, "main button", "运行本机检查")
        wait_page(page, 5000)
        click_text(page, "main button", "接受当前候选")
        wait_page(page, 2500)
        snap(page, "desktop-light-zh-accepted")

        # Dark theme (V02).
        page.evaluate("() => { document.documentElement.dataset.theme = 'dark'; }")
        wait_page(page, 400)
        snap(page, "desktop-dark-zh")

        # English light (V04).
        page.evaluate("() => { document.documentElement.dataset.theme = 'light'; }")
        click_text(page, ".controls button", "EN")
        wait_page(page, 800)
        snap(page, "desktop-light-en")

        # 390 mobile (V03) and 320.
        page.set_viewport_size({"width": 390, "height": 844})
        wait_page(page, 700)
        snap(page, "mobile-390-en")
        page.set_viewport_size({"width": 320, "height": 640})
        wait_page(page, 700)
        snap(page, "mobile-320-en")

        # 200% zoom (V06): 720x450 viewport ≈ 1440x900 at 200%.
        page.set_viewport_size({"width": 720, "height": 450})
        page.evaluate("() => { document.body.style.zoom = '2'; }")
        wait_page(page, 500)
        snap(page, "zoom-200-en")
        page.evaluate("() => { document.body.style.zoom = ''; }")

        # Keyboard: focus order + modal focus return.
        page.set_viewport_size({"width": 1440, "height": 900})
        wait_page(page, 500)
        click_text(page, "main button", "Capture candidate")
        wait_page(page, 1200)
        focused_in_modal = page.evaluate(
            "() => !!document.activeElement && !!document.activeElement.closest('.modal')"
        )
        page.keyboard.press("Escape")
        wait_page(page, 600)
        modal_gone = page.evaluate("() => !document.querySelector('.modal')")
        snap(page, "keyboard-modal-returned-en")
        print("modal focus inside:", focused_in_modal, "closed:", modal_gone)

        # Reduced motion: emulation + idle animation check.
        ctx2 = browser.new_context(
            locale="zh-CN",
            viewport={"width": 1440, "height": 900},
            reduced_motion="reduce",
        )
        page2 = ctx2.new_page()
        page2.goto(base)
        page2.wait_for_load_state("domcontentloaded")
        wait_page(page2, 1500)
        idle_fps = page2.evaluate(
            """() => new Promise(res => {
                let frames = 0;
                const tick = () => { frames++; if (frames < 600) requestAnimationFrame(tick); };
                requestAnimationFrame(tick);
                setTimeout(() => res(frames), 1000);
            })"""
        )
        print("reduced-motion idle frames in 1s:", idle_fps)
        snap(page2, "reduced-motion-zh")

        print("PAGE_ERRORS:", json.dumps(errors))
        print("EXTERNAL_REQUESTS:", json.dumps(external))
        print("SHOTS:", json.dumps(shots, indent=1))
        browser.close()


if __name__ == "__main__":
    exe, port, token, out_dir = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4]
    run(exe, port, token, out_dir)
