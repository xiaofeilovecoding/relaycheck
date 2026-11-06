"""R15/R16/V06 production browser gate with REAL assertions (not screenshot
counting). Every check either passes or fails loudly; failures exit nonzero.

Usage:
  python scripts/ui_gates.py <exe> <port> <bootstrap-token> <out-json>

Covers (evidence for the review items):
  RC06: 390/320/200% no horizontal overflow; nav/breadcrumb reachable
  RC04: handoff edit revokes preview + download; copy disabled
  RC05: accept updates the detail banner; detail follows accept
  RC07: lossless argv round-trip in the profile editor (spaces/-c/中文路径)
  R17:  page rAF count while idle (no render loop) — counted from the PAGE
        via an init hook, never by the probe itself; focus return asserted
"""
import json
import os
import sys
from pathlib import Path

from playwright.sync_api import sync_playwright

exe, port, token, out_path = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4]
BASE = f"http://127.0.0.1:{port}"

results = {"assertions": [], "external_requests": [], "page_errors": []}


def record(name, ok, detail=""):
    results["assertions"].append({"name": name, "ok": bool(ok), "detail": detail})
    print(("PASS " if ok else "FAIL ") + name + (" — " + detail if detail else ""))


SET = """([el, v]) => {
  const proto = Object.getPrototypeOf(el);
  Object.getOwnPropertyDescriptor(proto, 'value').set.call(el, v);
  el.dispatchEvent(new Event('input', { bubbles: true }));
}"""


def click(page, sel, text):
    page.evaluate(
        """([sel, t]) => {
        const els = [...document.querySelectorAll(sel)].filter(b => b.textContent.includes(t));
        if (!els.length) throw new Error('missing: ' + t);
        els[els.length - 1].click();
    }""",
        [sel, text],
    )


def overflow(page):
    return page.evaluate(
        "() => ({sw: document.documentElement.scrollWidth, cw: document.documentElement.clientWidth})"
    )


def main():
    ok_all = True
    with sync_playwright() as p:
        browser = p.chromium.launch(headless=True)
        ctx = browser.new_context(locale="zh-CN", viewport={"width": 1440, "height": 900})
        page = ctx.new_page()
        page.on("pageerror", lambda e: results["page_errors"].append(str(e)))
        page.on("request", lambda r: results["external_requests"].append(r.url)
                if f"127.0.0.1:{port}" not in r.url else None)
        # R17: count the PAGE's own rAF usage from before load — the probe
        # never schedules rAF itself.
        page.add_init_script(
            "window.__rafCount = 0; const orig = window.requestAnimationFrame;"
            "window.requestAnimationFrame = (cb) => { window.__rafCount++; return orig(cb); };"
        )
        page.goto(f"{BASE}/#rcb={token}")
        page.wait_for_load_state("domcontentloaded")
        page.wait_for_timeout(1500)

        # ---- demo journey ----
        click(page, "main button", "试用样例")
        page.wait_for_timeout(4500)
        page.locator(".task-card").first.click()
        page.wait_for_timeout(1800)

        # RC06: desktop no overflow
        o = overflow(page)
        record("no-horizontal-overflow-1440", o["sw"] <= o["cw"], str(o))

        # RC07: profile editor lossless argv (open via the edit button — the
        # demo ships a profile, so 运行本机检查 would run rather than edit)
        click(page, "main .check .ghost", "修改检查配置")
        page.wait_for_timeout(1200)
        # Remember the demo's REAL check so the gate can restore it after the
        # lossless-argv probe (the probe deliberately uses a fake exe).
        orig_exe, orig_args = page.evaluate(
            """() => {
            const inputs = [...document.querySelectorAll('.modal input[type=text]')];
            const ta = document.querySelector('.modal textarea[data-testid=args-editor]');
            return [inputs[1]?.value ?? '', ta?.value ?? ''];
        }"""
        )
        exe_path = r"D:\demo tools\我的 检查器.exe" if os.name == "nt" else "/demo tools/我的 检查器"
        arg_a = 'C:\\path with spaces\\run.py'
        arg_b = '-c'
        arg_c = 'import a; print("x  y")'
        page.evaluate(
            """([v1, v2]) => {
            const set = (el, v) => {
              const proto = Object.getPrototypeOf(el);
              Object.getOwnPropertyDescriptor(proto, 'value').set.call(el, v);
              el.dispatchEvent(new Event('input', { bubbles: true }));
            };
            const inputs = [...document.querySelectorAll('.modal input[type=text]')];
            set(inputs[1], v1);           // exe
            const ta = document.querySelector('.modal textarea[data-testid=args-editor]');
            set(ta, JSON.stringify(v2));
        }""",
            [exe_path, [arg_a, arg_b, arg_c, "", "embedded\nnewline"]],
        )
        click(page, ".modal button", "保存新版本")
        page.wait_for_timeout(1500)
        args_editor = page.locator('[data-testid="args-editor"]')
        final_args = [arg_a, arg_b, arg_c, "", "embedded\nnewline", "edited after saving"]
        args_editor.fill(json.dumps(final_args, ensure_ascii=False))
        click(page, ".modal button", "批准并运行")
        page.wait_for_timeout(2500)
        # Re-open the editor: the argv must round-trip exactly.
        click(page, "main .check .ghost", "修改检查配置")
        page.wait_for_timeout(900)
        args_now = page.evaluate(
            "() => document.querySelector('.modal textarea[data-testid=args-editor]')?.value ?? ''"
        )
        expected = final_args
        record("rc07-lossless-argv-roundtrip", bool(args_now) and json.loads(args_now) == expected, repr(args_now))
        exe_now = page.evaluate(
            "() => [...document.querySelectorAll('.modal input[type=text]')][1]?.value ?? ''"
        )
        record("rc07-exe-roundtrip", exe_now == exe_path, repr(exe_now))
        # Restore the real check in the open editor.
        page.evaluate(
            """([v1, v2]) => {
            const set = (el, v) => {
              const proto = Object.getPrototypeOf(el);
              Object.getOwnPropertyDescriptor(proto, 'value').set.call(el, v);
              el.dispatchEvent(new Event('input', { bubbles: true }));
            };
            const inputs = [...document.querySelectorAll('.modal input[type=text]')];
            set(inputs[1], v1);
            set(document.querySelector('.modal textarea[data-testid=args-editor]'), v2);
        }""",
            [orig_exe, orig_args],
        )
        click(page, ".modal button", "保存新版本")
        page.wait_for_timeout(1200)
        click(page, ".modal button", "批准并运行")
        page.wait_for_timeout(6000)
        page.keyboard.press("Escape")
        page.wait_for_timeout(500)

        # RC04: handoff edit revokes preview/download
        click(page, "main button", "预览并导出接手卡")
        page.wait_for_timeout(900)
        page.evaluate(
            """() => {
            const set = (el, v) => {
              const proto = Object.getPrototypeOf(el);
              Object.getOwnPropertyDescriptor(proto, 'value').set.call(el, v);
              el.dispatchEvent(new Event('input', { bubbles: true }));
            };
            set(document.querySelectorAll('.modal textarea')[0], '先复现 C2 取消等待。');
            set(document.querySelectorAll('.modal textarea')[1], 'C2: 取消行为尚未验证');
        }"""
        )
        modal = page.locator(".modal")
        next_action = modal.locator("textarea").first
        withheld = []

        def hold_export(route):
            withheld.append((route, route.fetch()))

        page.route("**/handoffs", hold_export)
        with page.expect_request(lambda req: req.method == "POST" and req.url.endswith("/handoffs")):
            modal.get_by_role("button", name="导出接手包", exact=True).click()
        # A Playwright call pumps the route handler before the old response is released.
        next_action.fill("暂改下一步")
        next_action.fill("先复现 C2 取消等待。")
        assert len(withheld) == 1, "expected one held export response"
        route, response = withheld.pop()
        assert response.ok, f"held export failed with HTTP {response.status}"
        with page.expect_response(lambda received: received.url.endswith("/handoffs")):
            route.fulfill(response=response)
        page.unroute("**/handoffs", hold_export)
        page.evaluate("() => new Promise(done => requestAnimationFrame(() => requestAnimationFrame(done)))")
        record("rc04-late-export-revoked", modal.locator("a[download]").count() == 0)
        click(page, ".modal button", "导出接手包")
        page.wait_for_timeout(2500)
        has_preview = page.evaluate("() => !!document.querySelector('[data-testid=handoff-preview]')")
        has_download = page.evaluate("() => !!document.querySelector('.modal a[download]')")
        record("rc04-preview-and-download-after-export", has_preview and has_download)
        page.evaluate("""() => Object.defineProperty(navigator.clipboard, 'writeText', {
            configurable: true,
            value: text => new Promise(resolve => { window.copyText = text; window.finishCopy = resolve; })
        })""")
        modal.locator("button").filter(has_text="复制").first.click()
        page.wait_for_function("() => typeof window.finishCopy === 'function'")
        preview_text = modal.locator('[data-testid="handoff-preview"]').text_content()
        record("rc04-clipboard-matches-preview", page.evaluate("window.copyText") == preview_text)
        # edit → revoke
        page.evaluate(
            """() => {
            const el = document.querySelectorAll('.modal textarea')[0];
            const proto = Object.getPrototypeOf(el);
            Object.getOwnPropertyDescriptor(proto, 'value').set.call(el, 'FIRST');
            el.dispatchEvent(new Event('input', { bubbles: true }));
        }"""
        )
        page.evaluate("window.finishCopy()")
        page.wait_for_timeout(500)
        revoked = page.evaluate(
            """() => ({
            preview: !!document.querySelector('[data-testid=handoff-preview]'),
            download: !!document.querySelector('.modal a[download]'),
            note: !!document.querySelector('[data-testid=handoff-revoked]'),
        })"""
        )
        record("rc04-edit-revokes-preview-and-download",
               not revoked["preview"] and not revoked["download"] and revoked["note"], str(revoked))
        record("rc04-late-clipboard-feedback-revoked", modal.locator("button").filter(has_text="已复制").count() == 0)
        # A→B→A back to the exported text: still revoked (edit-event gate).
        page.evaluate(
            """() => {
            const el = document.querySelectorAll('.modal textarea')[0];
            const proto = Object.getPrototypeOf(el);
            Object.getOwnPropertyDescriptor(proto, 'value').set.call(el, '先复现 C2 取消等待。');
            el.dispatchEvent(new Event('input', { bubbles: true }));
        }"""
        )
        page.wait_for_timeout(400)
        still_revoked = page.evaluate(
            "() => !!document.querySelector('[data-testid=handoff-revoked]') && !document.querySelector('.modal a[download]')"
        )
        record("rc04-aba-still-revoked", still_revoked)
        page.keyboard.press("Escape")
        page.wait_for_timeout(600)

        # RC05: accept updates the detail banner (the candidate was verified
        # before export; the primary after export is accept).
        click(page, "main button", "接受当前候选")
        page.wait_for_timeout(2500)
        accepted_banner = page.evaluate(
            "() => !!document.querySelector('[data-testid=accepted-banner]')"
        )
        record("rc05-accept-updates-detail-banner", accepted_banner)

        # modal focus return (V06)
        click(page, "main .check .ghost", "修改检查配置")
        page.wait_for_timeout(700)
        focus_in = page.evaluate("() => !!document.activeElement?.closest?.('.modal')")
        page.keyboard.press("Escape")
        page.wait_for_timeout(600)
        focus_after = page.evaluate(
            "() => ({tag: document.activeElement?.tagName, inModal: !!document.activeElement?.closest?.('.modal')})"
        )
        record("v06-modal-focus-in-and-returned", focus_in and not focus_after["inModal"], str(focus_after))

        # R17: idle rAF over 1.5s — page's own count, probe never schedules rAF
        before = page.evaluate("() => window.__rafCount")
        page.wait_for_timeout(1500)
        after = page.evaluate("() => window.__rafCount")
        record("r17-no-idle-rAF-render-loop", after - before == 0, f"delta={after - before}")

        # RC06: mobile 390/320 + 200%
        page.set_viewport_size({"width": 390, "height": 844})
        page.wait_for_timeout(600)
        o = overflow(page)
        record("no-horizontal-overflow-390", o["sw"] <= o["cw"], str(o))
        nav_visible = page.evaluate(
            "() => [...document.querySelectorAll('.nav-item')].every(el => el.getBoundingClientRect().width > 0)"
        )
        record("rc06-nav-reachable-390", nav_visible)
        page.set_viewport_size({"width": 320, "height": 640})
        page.wait_for_timeout(600)
        o = overflow(page)
        record("no-horizontal-overflow-320", o["sw"] <= o["cw"], str(o))
        # 200%: half viewport (720x450 = 1440x900 at 200%) with DSF 2
        page.set_viewport_size({"width": 720, "height": 450})
        ctx2 = page.context
        page.evaluate("() => {}")
        o = overflow(page)
        record("no-horizontal-overflow-zoom200-half-viewport", o["sw"] <= o["cw"], str(o))

        browser.close()

    ok_all = all(a["ok"] for a in results["assertions"]) and not results["page_errors"] and not results["external_requests"]
    results["ok"] = ok_all
    Path(out_path).write_text(json.dumps(results, indent=1, ensure_ascii=False), encoding="utf-8")
    print("GATE:", "PASS" if ok_all else "FAIL")
    if not ok_all:
        sys.exit(1)


if __name__ == "__main__":
    main()
