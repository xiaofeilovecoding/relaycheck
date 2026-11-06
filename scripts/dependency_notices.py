"""Collect the license texts accompanying the native and embedded dependencies."""

import json
from pathlib import Path
import re
import subprocess
import sys


ROOT = Path(__file__).resolve().parent.parent


def license_files(directory, explicit=None):
    pattern = re.compile(r"^(?:licen[cs]es?|copying|notice)(?:[._-]|$)", re.I)
    paths = {p for p in directory.iterdir() if p.is_file() and pattern.match(p.name)}
    if explicit:
        paths.add(directory / explicit)
    return sorted(paths)


def sections(directory, name, license_id, explicit=None):
    files = license_files(directory, explicit)
    if not files and name in {"include_dir 0.7.4", "include_dir_macros 0.7.4"}:
        # The crates omit the shared license included in their source commit.
        files = [ROOT / "licenses/include_dir-0.7.4.txt"]
    if not files:
        raise SystemExit(f"Review missing license text for {name} ({license_id})")
    return [f"{name} · {license_id or 'see attached terms'}\n" + "\n\n".join(
        f"--- {p.name} ---\n{p.read_text(encoding='utf-8', errors='replace')}" for p in files
    )]


def main(output):
    rustc = subprocess.check_output(["rustc", "-vV"], text=True, cwd=ROOT)
    target = next(line.removeprefix("host: ") for line in rustc.splitlines() if line.startswith("host: "))
    data = json.loads(subprocess.check_output([
        "cargo", "metadata", "--locked", "--format-version", "1", "--filter-platform", target
    ], text=True, encoding="utf-8", cwd=ROOT))
    resolved = {node["id"] for node in data["resolve"]["nodes"]}
    parts = ["RelayCheck third-party license texts"]
    for package in sorted(data["packages"], key=lambda p: (p["name"], p["version"])):
        if package["source"] and package["id"] in resolved:
            parts += sections(Path(package["manifest_path"]).parent,
                              f'{package["name"]} {package["version"]}',
                              package["license"], package["license_file"])
    for name in ("react", "react-dom", "scheduler"):
        directory = ROOT / "web/node_modules" / name
        package = json.loads((directory / "package.json").read_text(encoding="utf-8"))
        parts += sections(directory, f'{name} {package["version"]}', package.get("license"))
    output.write_text("\n\n".join(parts) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main(Path(sys.argv[1]))
