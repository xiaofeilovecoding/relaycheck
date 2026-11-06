"""Run Chromium checks against a production binary in an isolated data directory."""

import argparse
import os
from pathlib import Path
import queue
import re
import subprocess
import sys
import tempfile
import threading
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("executable", type=Path)
    parser.add_argument("report", type=Path)
    args = parser.parse_args()
    executable = args.executable.resolve(strict=True)
    report = args.report.resolve()
    report.parent.mkdir(parents=True, exist_ok=True)
    gate = Path(__file__).with_name("ui_gates.py").resolve()
    env = dict(os.environ, PYTHONUTF8="1")
    env["PATH"] = str(Path(sys.executable).parent) + os.pathsep + env.get("PATH", "")
    with tempfile.TemporaryDirectory(prefix="relaycheck-browser-") as directory:
        with subprocess.Popen(
            [str(executable), "serve", "--data-dir", directory], cwd=directory,
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True,
            encoding="utf-8", env=env,
        ) as server:
            lines = queue.Queue()

            def collect():
                for line in server.stdout:
                    lines.put(line)
                lines.put(None)

            threading.Thread(target=collect, daemon=True).start()
            try:
                deadline = time.monotonic() + 15
                while time.monotonic() < deadline:
                    line = lines.get(timeout=max(0.1, deadline - time.monotonic()))
                    if line is None:
                        raise SystemExit("RelayCheck exited before startup")
                    match = re.search(r"http://127\.0\.0\.1:(\d+)/#rcb=([a-f0-9]+)", line)
                    if match:
                        port, token = match.groups()
                        break
                else:
                    raise SystemExit("RelayCheck startup timed out")
                result = subprocess.run(
                    [sys.executable, str(gate), str(executable), port, token, str(report)],
                    cwd=directory, env=env, timeout=150,
                )
                return result.returncode
            except queue.Empty:
                raise SystemExit("RelayCheck startup timed out") from None
            except subprocess.TimeoutExpired:
                raise SystemExit("Browser gate timed out") from None
            finally:
                if server.poll() is None:
                    server.terminate()
                    try:
                        server.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        server.kill()
                        server.wait()


if __name__ == "__main__":
    raise SystemExit(main())
