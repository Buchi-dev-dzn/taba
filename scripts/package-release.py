#!/usr/bin/env python3
"""Build archives from already-built binaries, and verify candidate assets."""

import hashlib
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import zipfile

ROOT = Path(__file__).resolve().parent.parent
TARGETS = (
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
)
VERSION = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["package"]["version"]


def filename(target):
    ext = "zip" if "windows" in target else "tar.gz"
    return f"taba-v{VERSION}-{target}.{ext}"


def checksum(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def package(target, out):
    if target not in TARGETS:
        raise SystemExit(f"unknown target: {target}")
    windows = "windows" in target
    exe = "taba.exe" if windows else "taba"
    binary = ROOT / "target" / target / "release" / exe
    inputs = {exe: binary, "LICENSE": ROOT / "LICENSE", "README.md": ROOT / "README.md"}
    out.mkdir(parents=True, exist_ok=True)
    path = out / filename(target)
    with tempfile.TemporaryDirectory() as temp:
        # Check the archived bytes and run the extracted binary, not the build-tree binary.
        dest = Path(temp)
        if windows:
            with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED) as archive:
                for name, source in inputs.items():
                    archive.write(source, name)
            with zipfile.ZipFile(path) as archive:
                if archive.namelist() != list(inputs):
                    raise SystemExit(f"unexpected ZIP contents: {path}")
                for name in inputs:
                    (dest / name).write_bytes(archive.read(name))
        else:
            with tarfile.open(path, "w:gz") as archive:
                for name, source in inputs.items():
                    archive.add(source, arcname=name, recursive=False)
            with tarfile.open(path, "r:gz") as archive:
                members = archive.getmembers()
                if [m.name for m in members] != list(inputs) or not all(m.isfile() for m in members):
                    raise SystemExit(f"unexpected tar contents: {path}")
                archive.extractall(dest, filter="data")
        for name, source in inputs.items():
            if checksum(dest / name) != checksum(source):
                raise SystemExit(f"archive member differs from source: {name}")
        version = subprocess.check_output([str(dest / exe), "--version"], text=True, timeout=10).strip()
        if version != f"taba {VERSION}":
            raise SystemExit(f"unexpected binary version: {version!r}")
    digest = checksum(path)
    (out / f"{path.name}.sha256").write_text(f"{digest}  {path.name}\n", encoding="ascii", newline="\n")
    print(f"{version}: {digest}  {path.name}")


def assemble(source, dest, commit):
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise SystemExit("expected a full commit SHA")
    expected = {filename(t) for t in TARGETS}
    expected |= {f"{name}.sha256" for name in expected}
    if {p.name for p in source.iterdir()} != expected:
        raise SystemExit("candidate does not contain exactly the expected archives and sidecars")
    dest.mkdir(parents=True, exist_ok=True)
    sums = []
    for target in sorted(TARGETS, key=filename):
        name = filename(target)
        path = source / name
        line = f"{checksum(path)}  {name}\n"
        if (source / f"{name}.sha256").read_text(encoding="ascii") != line:
            raise SystemExit(f"checksum mismatch: {name}")
        sums.append(line)
        shutil.copyfile(path, dest / name)
    (dest / "SHA256SUMS").write_text("".join(sums), encoding="ascii", newline="\n")
    (dest / "COMMIT").write_text(f"{commit}\n", encoding="ascii", newline="\n")
    print("".join(sums), end="")


if __name__ == "__main__":
    if len(sys.argv) == 4 and sys.argv[1] == "package":
        package(sys.argv[2], Path(sys.argv[3]))
    elif len(sys.argv) == 5 and sys.argv[1] == "assemble":
        assemble(Path(sys.argv[2]), Path(sys.argv[3]), sys.argv[4])
    else:
        raise SystemExit("usage: package-release.py package TARGET OUT | assemble INPUT OUT COMMIT")
