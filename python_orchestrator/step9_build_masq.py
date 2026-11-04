#!/usr/bin/env python3
"""Step9: Build massq CLI binary and massq_py Python extension module.

Builds two artefacts:
  1. target/release/massq       -- Rust CLI binary for one-off terminal queries
  2. massq_py Python module      -- PyO3 extension installed into python_orchestrator/.venv

Usage:
    # Build both (default)
    python3 step9_build_masq.py

    # Build only the Rust CLI binary
    python3 step9_build_masq.py --rust-only

    # Build only the Python module
    python3 step9_build_masq.py --python-only

    # Install Python module in release (optimised) mode
    python3 step9_build_masq.py --python-only --release
"""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
from pathlib import Path

# -- Path resolution -----------------------------------------------------------

# This script lives in  <project_root>/python_orchestrator/
SCRIPT_DIR = Path(__file__).resolve().parent
PROJECT_ROOT = SCRIPT_DIR.parent
VENV_DIR = SCRIPT_DIR / ".venv"


# -- Helpers -------------------------------------------------------------------

def header(text: str) -> None:
    print(f"\n{'=' * 55}")
    print(f"  {text}")
    print(f"{'=' * 55}")


def ok(text: str) -> None:
    print(f"[OK] {text}")


def info(text: str) -> None:
    print(f"[*]  {text}")


def fail(text: str) -> None:
    print(f"[!]  {text}", file=sys.stderr)


def find_maturin() -> str:
    """Return path to maturin executable, or exit with a helpful message."""
    found = shutil.which("maturin")
    if found:
        return found

    # uv tool install puts binaries here
    candidate = Path.home() / ".local" / "bin" / "maturin"
    if candidate.exists():
        return str(candidate)

    fail("maturin not found on PATH and not in ~/.local/bin/maturin")
    fail("Install it with:  uv tool install 'maturin[patchelf]'")
    sys.exit(1)


def run(cmd: list, env: dict | None = None, cwd: Path = PROJECT_ROOT) -> None:
    """Run a subprocess, stream output live, and exit on failure."""
    display = " ".join(str(c) for c in cmd)
    info(f"$ {display}")
    result = subprocess.run(cmd, cwd=cwd, env=env)
    if result.returncode != 0:
        fail(f"Command failed (exit {result.returncode}): {display}")
        sys.exit(result.returncode)


# -- Build steps ---------------------------------------------------------------

def build_rust_massq() -> None:
    """Compile the massq CLI binary with cargo."""
    header("BUILD: massq CLI binary (cargo build --release)")

    run(["cargo", "build", "--release", "--bin", "massq"])

    binary = PROJECT_ROOT / "target" / "release" / "massq"
    if not binary.exists():
        fail(f"Expected binary not found after build: {binary}")
        sys.exit(1)

    ok(f"massq binary: {binary}")


def build_python_module(release: bool) -> None:
    """Compile and install the massq_py PyO3 extension into the project venv."""
    mode = "build --release" if release else "develop"
    header(f"BUILD: massq_py Python extension (maturin {mode})")

    maturin = find_maturin()
    info(f"maturin: {maturin}")

    if not VENV_DIR.exists():
        fail(f"Virtual environment not found: {VENV_DIR}")
        fail("Create it first:  python3 -m venv python_orchestrator/.venv")
        sys.exit(1)

    info(f"venv:    {VENV_DIR}")

    # Point maturin at the project venv via VIRTUAL_ENV so it installs there
    # regardless of which shell venv is currently activated.
    env = os.environ.copy()
    env["VIRTUAL_ENV"] = str(VENV_DIR)
    # Remove CONDA_PREFIX if present -- it can confuse maturin's venv detection.
    env.pop("CONDA_PREFIX", None)

    if release:
        # maturin build --release produces a wheel, then we pip-install it.
        wheel_dir = PROJECT_ROOT / "target" / "wheels"
        run([
            maturin, "build", "--release",
            "--features", "python",
            "--out", str(wheel_dir),
        ], env=env)

        wheels = sorted(wheel_dir.glob("massq_py-*.whl"))
        if not wheels:
            fail(f"No massq_py wheel found in {wheel_dir}")
            sys.exit(1)
        wheel = wheels[-1]
        info(f"Installing wheel: {wheel}")
        pip = VENV_DIR / "bin" / "pip"
        run([str(pip), "install", "--force-reinstall", str(wheel)])
        ok(f"massq_py installed from wheel: {wheel.name}")
    else:
        # maturin develop installs an editable build directly into the venv.
        run([maturin, "develop", "--features", "python"], env=env)
        ok(f"massq_py installed (editable) into {VENV_DIR}")

    # Quick sanity check: import the module from the venv.
    python = VENV_DIR / "bin" / "python"
    check = subprocess.run(
        [str(python), "-c", "import massq_py; print('classes:', [x for x in dir(massq_py) if not x.startswith('_')])"],
        capture_output=True,
        text=True,
    )
    if check.returncode == 0:
        ok(f"massq_py import OK -- {check.stdout.strip()}")
    else:
        fail("massq_py import check failed after build!")
        fail(check.stderr.strip())
        sys.exit(1)


# -- CLI -----------------------------------------------------------------------

def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Build massq CLI binary and/or massq_py Python module.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    group = parser.add_mutually_exclusive_group()
    group.add_argument(
        "--rust-only",
        action="store_true",
        help="Build only the massq Rust CLI binary.",
    )
    group.add_argument(
        "--python-only",
        action="store_true",
        help="Build only the massq_py Python extension module.",
    )
    parser.add_argument(
        "--release",
        action="store_true",
        help=(
            "Build the Python module in release (optimised) mode via "
            "maturin build + pip install. "
            "Default is editable/dev mode (maturin develop)."
        ),
    )
    return parser.parse_args()


def main() -> None:
    args = parse_args()

    build_rust = not args.python_only
    build_python = not args.rust_only

    info(f"Project root: {PROJECT_ROOT}")

    if build_rust:
        build_rust_massq()

    if build_python:
        build_python_module(release=args.release)

    print()
    print("*" * 55)
    if build_rust and build_python:
        print("[OK] Both artefacts built successfully.")
    elif build_rust:
        print("[OK] massq CLI binary built successfully.")
    else:
        print("[OK] massq_py Python module installed successfully.")
    print("*" * 55)


if __name__ == "__main__":
    main()
