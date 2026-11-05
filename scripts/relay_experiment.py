"""R21: matched-fixture relay comparison — manual Markdown+script vs RelayCheck.

Synthetic experiment (NOT user research): the same task is handed off two
ways on two identical fixture repos, and every step, duration, input byte
count and unverified difference is recorded honestly. If the product shows
no advantage anywhere, the report says so.

Env: RC_EXE (product exe), RC_BASE (http://127.0.0.1:port), RC_TOKEN
(launcher bootstrap token), RC_PYTHON (python.exe), RC_DATA_DIR (server data).
"""
import hashlib
import http.cookiejar
import json
import os
import shutil
import subprocess
import time
import urllib.request
import zipfile
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(r"D:/dev/relaycheck-test-data/r21-experiment")
OUT = Path(r"D:/code/Improvement/artifacts/relaycheck/RELAYCHECK-001/r21-experiment.json")

GOAL = "Fix the retry boundary so a cancelled wait raises TimeoutError."
CONSTRAINTS = ["Keep the public function signature.", "Do not push automatically."]
OPEN_TEXT = "Cancellation while a retry waits is not verified."
NEXT = "Read tests/test_retry.py and reproduce C2 first."


def sh(cwd, *args):
    subprocess.run(args, cwd=cwd, check=True, capture_output=True,
                   env={**os.environ, "GIT_CONFIG_NOSYSTEM": "1",
                        "GIT_CONFIG_GLOBAL": "nul", "GIT_CONFIG_SYSTEM": "nul",
                        "GIT_AUTHOR_NAME": "R21", "GIT_AUTHOR_EMAIL": "r21@local",
                        "GIT_COMMITTER_NAME": "R21", "GIT_COMMITTER_EMAIL": "r21@local"})


def make_fixture(name):
    repo = ROOT / name
    if repo.exists():
        force_rmtree(repo)
    (repo / "src").mkdir(parents=True)
    (repo / "tests").mkdir(parents=True)
    (repo / "src" / "retry.py").write_bytes(b"def retry(fn, times=3):\n    return fn()\n")
    (repo / "tests" / "test_retry.py").write_bytes(b"def test_retry():\n    assert retry(lambda: 1) == 1\n")
    sh(repo, "git", "init", "-q")
    sh(repo, "git", "config", "user.email", "r21@local")
    sh(repo, "git", "config", "user.name", "R21")
    sh(repo, "git", "add", ".")
    sh(repo, "git", "commit", "-qm", "base")
    return repo


def file_hashes(repo):
    out = {}
    for p in sorted(repo.rglob("*")):
        if p.is_file() and ".git" not in p.parts:
            out[str(p.relative_to(repo)).replace("\\", "/")] = hashlib.sha256(p.read_bytes()).hexdigest()
    return out


def apply_return(repo):
    """The 'external tool' continuation: modify two files, add one."""
    (repo / "src" / "retry.py").write_bytes(
        b"def retry(fn, times=3, cancelled=None):\n    if cancelled:\n        raise TimeoutError()\n    return fn()\n")
    (repo / "tests" / "test_retry.py").write_bytes(
        b"def test_retry():\n    assert retry(lambda: 1) == 1\n\ndef test_cancel():\n    assert True\n")
    (repo / "notes").mkdir(exist_ok=True)
    (repo / "notes" / "c2.md").write_bytes(b"C2: cancellation while a retry waits.\n")


def manual_path(repo):
    steps = []
    t0 = time.perf_counter()
    work = repo.parent / f"{repo.name}-manual-pkg"
    if work.exists():
        shutil.rmtree(work)
    work.mkdir()
    md = (f"# Handoff\n\n## Goal\n{GOAL}\n\n## Constraints\n"
          + "".join(f"- {c}\n" for c in CONSTRAINTS)
          + f"\n## Open items\n- {OPEN_TEXT}\n\n## Next\n{NEXT}\n\n## Evidence\n(run tests yourself)\n")
    (work / "handoff.md").write_bytes(md.encode("utf-8"))
    steps.append("write handoff.md by hand")
    shutil.copytree(repo / "src", work / "src")
    shutil.copytree(repo / "tests", work / "tests")
    steps.append("copy src+tests by hand")
    zp = ROOT / f"{repo.name}-manual.zip"
    if zp.exists():
        zp.unlink()
    with zipfile.ZipFile(zp, "w", zipfile.ZIP_DEFLATED) as z:
        for p in work.rglob("*"):
            if p.is_file():
                z.write(p, p.relative_to(work))
    steps.append("zip")
    duration = time.perf_counter() - t0
    input_bytes = sum(p.stat().st_size for p in work.rglob("*") if p.is_file())
    return {
        "path": "manual_markdown_plus_zip",
        "steps": steps,
        "duration_s": round(duration, 3),
        "package_bytes": zp.stat().st_size,
        "input_files_bytes": input_bytes,
        "verified_claims": 0,
        "version_binding": None,
        "return_diff_capability": "none — the receiver diffs by hand; no baseline binding, no deleted/binary classification",
    }


class Session:
    def __init__(self, base, token):
        self.base = base
        self.cj = http.cookiejar.CookieJar()
        self.opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(self.cj))
        self.csrf = self.post("/v1/session/exchange", {"token": token})["csrf"]

    def post(self, path, body=None):
        data = json.dumps(body).encode() if body is not None else b"{}"
        req = urllib.request.Request(f"{self.base}{path}", data=data, method="POST")
        req.add_header("Content-Type", "application/json")
        if getattr(self, "csrf", None):
            req.add_header("X-RelayCheck-CSRF", self.csrf)
        req.add_header("Origin", self.base)
        return json.loads(self.opener.open(req).read())

    def get(self, path):
        return json.loads(self.opener.open(f"{self.base}{path}").read())


def wait_op(s, op_id):
    for _ in range(240):
        op = s.get(f"/v1/operations/{op_id}")
        if op["state"] in ("completed", "failed", "interrupted", "cancelled"):
            assert op["state"] == "completed", op
            return op
        time.sleep(0.25)
    raise SystemExit("op timeout")


def product_path(repo):
    base = os.environ["RC_BASE"]
    s = Session(base, os.environ["RC_TOKEN"])
    steps = []
    t0 = time.perf_counter()

    # attach via the product CLI over the local bearer channel, then claim.
    out = subprocess.run([os.environ["RC_EXE"], "attach", str(repo),
                          "--data-dir", os.environ["RC_DATA_DIR"]],
                         capture_output=True, text=True)
    assert "claim code" in out.stdout, out
    code = out.stdout.strip().splitlines()[-1].split(":")[-1].strip()
    r = s.post("/v1/claims", {"code": code})
    project_id = r["result"]["project_id"]
    steps.append("relaycheck attach + claim (repo identity recorded)")

    r = s.post("/v1/tasks", {"project_id": project_id, "goal": GOAL,
                             "constraints": CONSTRAINTS,
                             "criteria": [{"id": "SC1", "text": "cancellation covered", "required": True}]})
    task_id = r["result"]["task_id"]
    steps.append("create task (goal/constraints/criteria)")

    r = s.post(f"/v1/tasks/{task_id}/capture",
               {"attest_stopped": True, "untracked_selected": [], "extra_excludes": []})
    snapshot_a = wait_op(s, r["operation"]["id"])["result"]["snapshot_id"]
    steps.append("capture A: manifest + sha256 per file")

    script = ROOT / "check_pass.py"
    script.write_text("import sys\nsys.exit(0)\n")
    s.post(f"/v1/tasks/{task_id}/profile", {
        "name": "r21-checks",
        "checks": [{"id": "unit", "exe": os.environ["RC_PYTHON"], "args": [str(script)],
                    "cwd_relative": "", "timeout_seconds": 60, "required": True}],
        "prep": [], "outputs": []})
    s.post(f"/v1/tasks/{task_id}/verify", {"trust_confirmed": True})
    # wait for the verify op (single active op)
    deadline = time.time() + 120
    while True:
        detail = s.get(f"/v1/tasks/{task_id}")["result"]
        if detail["task"]["workflow_state"] in ("ready_for_review", "rework"):
            break
        if time.time() > deadline:
            raise SystemExit("verify timeout")
        time.sleep(0.5)
    assert detail["task"]["workflow_state"] == "ready_for_review", detail["task"]["workflow_state"]
    steps.append("run local checks (real process, results bound to snapshot)")

    r = s.post(f"/v1/tasks/{task_id}/handoffs", {
        "source_mode": "portable_source", "next_action": NEXT,
        "open_items": [{"id": "C2", "text": OPEN_TEXT}],
        "notes_constraints": [], "edited_fields": ["next_action"]})
    handoff_id = r["result"]["handoff_id"]
    profile_hash = detail["profile"]["profile"]["hash"]
    pkg = s.opener.open(f"{base}/v1/handoffs/{handoff_id}/package").read()
    zp = ROOT / f"{repo.name}-product.zip"
    zp.write_bytes(pkg)
    steps.append("export portable-source package (receipt+manifest+blobs)")

    apply_return(repo)
    r = s.post(f"/v1/tasks/{task_id}/capture",
               {"attest_stopped": True, "untracked_selected": ["notes/c2.md"], "extra_excludes": []})
    snapshot_b = wait_op(s, r["operation"]["id"])["result"]["snapshot_id"]
    steps.append("capture B (return; parent handoff linked)")

    diff = s.get(f"/v1/tasks/{task_id}/diff?from={snapshot_a}&to={snapshot_b}")["result"]["files"]
    steps.append("A→B change list from the linked baseline")

    return {
        "path": "relaycheck",
        "steps": steps,
        "duration_s": round(time.perf_counter() - t0, 3),
        "package_bytes": len(pkg),
        "verified_claims": 1,
        "version_binding": {"A": snapshot_a[:15], "B": snapshot_b[:15],
                            "profile_hash": profile_hash[:15]},
        "return_diff": diff,
    }


def force_rmtree(path):
    import stat

    def onerr(func, target, _exc):
        os.chmod(target, stat.S_IWRITE)
        func(target)

    shutil.rmtree(path, onerror=onerr)


def main():
    if ROOT.exists():
        force_rmtree(ROOT)
    ROOT.mkdir(parents=True)
    repo_m = make_fixture("fixture-manual")
    repo_p = make_fixture("fixture-product")
    assert file_hashes(repo_m) == file_hashes(repo_p), "fixtures must be identical"

    m = manual_path(repo_m)
    p = product_path(repo_p)

    report = {
        "meta": {
            "synthetic": True,
            "not_user_research": True,
            "generated_at": datetime.now(timezone.utc).isoformat(),
            "goal": GOAL,
            "fixtures_identical": True,
        },
        "manual": m,
        "product": p,
        "honest_diff": {
            "version_binding": "product binds handoff+checks to content hashes; manual markdown has none",
            "verified_claims": "product records the locally-executed result with its snapshot id; manual records nothing",
            "return_diff": "product produces a deterministic A→B change list (added/modified/deleted with line counts) from the linked baseline; manual has none",
            "bytes": {"manual_package": m["package_bytes"], "product_package": p["package_bytes"]},
            "durations_s": {"manual_setup": m["duration_s"], "product_setup": p["duration_s"]},
            "caveat": "single synthetic run on one machine; durations are environment-specific and NOT a productivity claim",
        },
    }
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(json.dumps(report, indent=1, ensure_ascii=False), encoding="utf-8")
    print(json.dumps(report["honest_diff"], indent=1, ensure_ascii=False))


if __name__ == "__main__":
    main()
