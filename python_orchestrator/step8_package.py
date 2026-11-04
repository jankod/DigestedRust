#!/usr/bin/env python3
"""Step8: package an already-built database directory for distribution.

This script creates a self-contained DB package with this structure:

<package_name>/
  manifest.toml
  checksums.sha256
  rocksdb/

The package does NOT include the runtime tool (massq), which is distributed separately.
"""

from __future__ import annotations

import argparse
import hashlib
import os
import shutil
import subprocess
import sys
import tempfile
import zipfile
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, Iterable, List

try:
    import tomllib
except ModuleNotFoundError as exc:  # pragma: no cover
    raise SystemExit("Python 3.11+ is required (missing tomllib)") from exc


DEFAULT_DB_REL_PATH = "final_db/rocksdb"
DEFAULT_MANIFEST_REL_PATH = "manifest.toml"
DEFAULT_OUTPUT_DIR_NAME = "release"
DEFAULT_DB_SCHEMA_VERSION = "1"
DEFAULT_MIN_MASSQ_VERSION = "0.1.0"
CHECKSUMS_FILE_NAME = "checksums.sha256"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Package built RocksDB database into release artifact(s)."
    )
    parser.add_argument(
        "--path-build-dir",
        default=None,
        help="Build directory with final_db/rocksdb and manifest.toml (or PATH_BUILD_DIR env).",
    )
    parser.add_argument(
        "--path-db-dir",
        default=DEFAULT_DB_REL_PATH,
        help="Path to RocksDB dir (relative to build dir unless absolute).",
    )
    parser.add_argument(
        "--path-manifest-file",
        default=DEFAULT_MANIFEST_REL_PATH,
        help="Path to source manifest.toml (relative to build dir unless absolute).",
    )
    parser.add_argument(
        "--path-output-dir",
        default=None,
        help="Output directory for release archives (default: <build_dir>/release).",
    )
    parser.add_argument(
        "--package-name",
        default=None,
        help="Root directory name inside archive (default: <build_dir_name>_db).",
    )
    parser.add_argument(
        "--formats",
        default="auto",
        help="Comma-separated formats: auto,tar.zst,zip (default: auto).",
    )
    parser.add_argument(
        "--db-schema-version",
        default=DEFAULT_DB_SCHEMA_VERSION,
        help="Manifest compatibility field: package.db_schema_version.",
    )
    parser.add_argument(
        "--min-massq-version",
        default=DEFAULT_MIN_MASSQ_VERSION,
        help="Manifest compatibility field: package.min_massq_version.",
    )
    parser.add_argument(
        "--keep-staging",
        action="store_true",
        help="Keep temporary staging directory for inspection.",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Prepare staging + checksums, skip archive creation.",
    )
    parser.add_argument(
        "--force",
        action="store_true",
        help="Overwrite existing archive files if present.",
    )
    return parser.parse_args()


def resolve_path(base: Path, candidate: str) -> Path:
    path = Path(candidate)
    return path if path.is_absolute() else base / path


def choose_formats(raw: str) -> List[str]:
    requested = [part.strip().lower() for part in raw.split(",") if part.strip()]
    if not requested:
        raise ValueError("--formats must not be empty")

    supported = {"auto", "tar.zst", "zip"}
    unknown = [fmt for fmt in requested if fmt not in supported]
    if unknown:
        raise ValueError(f"Unsupported format(s): {', '.join(unknown)}")

    if "auto" in requested:
        if len(requested) > 1:
            raise ValueError("When using auto, do not combine with explicit formats")
        if sys.platform.startswith("win"):
            return ["zip"]
        return ["tar.zst"]

    # Keep stable unique order.
    out: List[str] = []
    seen = set()
    for fmt in requested:
        if fmt not in seen:
            out.append(fmt)
            seen.add(fmt)
    return out


def flatten_dict(prefix: str, obj: Dict[str, Any], out: Dict[str, Any]) -> None:
    for key, value in obj.items():
        key_str = str(key)
        merged = f"{prefix}.{key_str}" if prefix else key_str
        if isinstance(value, dict):
            flatten_dict(merged, value, out)
        else:
            out[merged] = value


def _format_toml_value(value: Any) -> str:
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, float):
        return repr(value)
    if isinstance(value, list):
        return "[" + ", ".join(_format_toml_value(item) for item in value) + "]"
    text = str(value).replace("\\", "\\\\").replace('"', '\\"')
    return f'"{text}"'


def write_manifest_toml(path: Path, values: Dict[str, Any]) -> None:
    root: Dict[str, Any] = {}
    sections: Dict[str, Dict[str, Any]] = {}

    for key, value in sorted(values.items()):
        parts = key.split(".")
        if len(parts) == 1:
            root[parts[0]] = value
        else:
            section = ".".join(parts[:-1])
            leaf = parts[-1]
            sections.setdefault(section, {})[leaf] = value

    lines: List[str] = []
    for key, value in sorted(root.items()):
        lines.append(f"{key} = {_format_toml_value(value)}")

    for section in sorted(sections.keys()):
        if lines:
            lines.append("")
        lines.append(f"[{section}]")
        for key, value in sorted(sections[section].items()):
            lines.append(f"{key} = {_format_toml_value(value)}")

    path.write_text("\n".join(lines) + ("\n" if lines else ""), encoding="utf-8")


def sha256_of_file(path: Path) -> str:
    hasher = hashlib.sha256()
    with path.open("rb") as fh:
        while True:
            chunk = fh.read(1024 * 1024)
            if not chunk:
                break
            hasher.update(chunk)
    return hasher.hexdigest()


def iter_files_sorted(root: Path) -> Iterable[Path]:
    files = [p for p in root.rglob("*") if p.is_file()]
    files.sort(key=lambda p: p.as_posix())
    return files


def write_checksums_file(package_root: Path) -> Path:
    checksum_path = package_root / CHECKSUMS_FILE_NAME

    lines: List[str] = []
    for file_path in iter_files_sorted(package_root):
        if file_path == checksum_path:
            continue
        rel = file_path.relative_to(package_root).as_posix()
        digest = sha256_of_file(file_path)
        lines.append(f"{digest}  {rel}")

    checksum_path.write_text("\n".join(lines) + ("\n" if lines else ""), encoding="utf-8")
    return checksum_path


def update_packaged_manifest(
    source_manifest: Path,
    destination_manifest: Path,
    db_schema_version: str,
    min_massq_version: str,
) -> None:
    parsed = tomllib.loads(source_manifest.read_text(encoding="utf-8"))
    flat: Dict[str, Any] = {}
    flatten_dict("", parsed, flat)

    flat["package.created_at"] = datetime.now(timezone.utc).isoformat()
    flat["package.db_schema_version"] = db_schema_version
    flat["package.min_massq_version"] = min_massq_version
    flat["package.checksums_file"] = CHECKSUMS_FILE_NAME

    # Ensure runtime lookup convention remains explicit after extraction.
    flat["storage.path"] = "rocksdb"

    write_manifest_toml(destination_manifest, flat)


def create_zip_archive(archive_path: Path, staging_parent: Path, root_name: str) -> None:
    root_path = staging_parent / root_name
    with zipfile.ZipFile(
        archive_path,
        mode="w",
        compression=zipfile.ZIP_DEFLATED,
        compresslevel=9,
    ) as zf:
        for file_path in iter_files_sorted(root_path):
            rel = file_path.relative_to(staging_parent).as_posix()
            zf.write(file_path, arcname=rel)


def create_tar_zst_archive(archive_path: Path, staging_parent: Path, root_name: str) -> None:
    tar_bin = shutil.which("tar")
    if not tar_bin:
        raise RuntimeError("tar is not available on PATH (required for tar.zst)")

    cmd = [tar_bin, "--zstd", "-cf", str(archive_path), "-C", str(staging_parent), root_name]
    result = subprocess.run(cmd, capture_output=True, text=True)
    if result.returncode != 0:
        stderr = result.stderr.strip()
        raise RuntimeError(f"tar --zstd failed: {stderr or 'unknown error'}")


def write_archive_sha256(archive_path: Path) -> Path:
    digest = sha256_of_file(archive_path)
    out_path = archive_path.with_suffix(archive_path.suffix + ".sha256")
    out_path.write_text(f"{digest}  {archive_path.name}\n", encoding="utf-8")
    return out_path


def package_db(args: argparse.Namespace) -> int:
    build_dir_raw = args.path_build_dir or os.environ.get("PATH_BUILD_DIR")
    if not build_dir_raw:
        print("[!] Missing --path-build-dir (or PATH_BUILD_DIR env)", file=sys.stderr)
        return 2

    build_dir = Path(build_dir_raw).resolve()
    if not build_dir.exists():
        print(f"[!] Build dir does not exist: {build_dir}", file=sys.stderr)
        return 2

    db_dir = resolve_path(build_dir, args.path_db_dir).resolve()
    manifest_file = resolve_path(build_dir, args.path_manifest_file).resolve()
    output_dir = (
        resolve_path(build_dir, args.path_output_dir)
        if args.path_output_dir
        else (build_dir / DEFAULT_OUTPUT_DIR_NAME)
    ).resolve()

    package_name = args.package_name or f"{build_dir.name}_db"
    formats = choose_formats(args.formats)

    if not db_dir.exists() or not db_dir.is_dir():
        print(f"[!] RocksDB dir is missing: {db_dir}", file=sys.stderr)
        return 2
    if not manifest_file.exists() or not manifest_file.is_file():
        print(f"[!] manifest.toml is missing: {manifest_file}", file=sys.stderr)
        return 2

    output_dir.mkdir(parents=True, exist_ok=True)

    tmp_dir = Path(tempfile.mkdtemp(prefix="step8_pack_", dir=str(output_dir)))
    package_root = tmp_dir / package_name
    rocksdb_dest = package_root / "rocksdb"
    packaged_manifest = package_root / "manifest.toml"

    print(f"[*] Build dir: {build_dir}")
    print(f"[*] RocksDB source: {db_dir}")
    print(f"[*] Manifest source: {manifest_file}")
    print(f"[*] Output dir: {output_dir}")
    print(f"[*] Package root name: {package_name}")

    try:
        package_root.mkdir(parents=True, exist_ok=False)

        # LOCK file is runtime-local and should not be distributed.
        shutil.copytree(db_dir, rocksdb_dest, ignore=shutil.ignore_patterns("LOCK"))
        update_packaged_manifest(
            source_manifest=manifest_file,
            destination_manifest=packaged_manifest,
            db_schema_version=args.db_schema_version,
            min_massq_version=args.min_massq_version,
        )
        checksums_path = write_checksums_file(package_root)
        print(f"[*] Wrote checksums: {checksums_path}")

        generated_archives: List[Path] = []

        if args.dry_run:
            print("[*] Dry-run enabled: skipping archive creation")
        else:
            for fmt in formats:
                if fmt == "zip":
                    archive_path = output_dir / f"{package_name}.zip"
                    if archive_path.exists() and not args.force:
                        raise RuntimeError(f"Archive already exists: {archive_path} (use --force)")
                    create_zip_archive(archive_path, tmp_dir, package_name)
                elif fmt == "tar.zst":
                    archive_path = output_dir / f"{package_name}.tar.zst"
                    if archive_path.exists() and not args.force:
                        raise RuntimeError(f"Archive already exists: {archive_path} (use --force)")
                    create_tar_zst_archive(archive_path, tmp_dir, package_name)
                else:  # pragma: no cover
                    raise RuntimeError(f"Unexpected format: {fmt}")

                generated_archives.append(archive_path)
                archive_sha = write_archive_sha256(archive_path)
                print(f"[*] Created archive: {archive_path}")
                print(f"[*] Created archive checksum: {archive_sha}")

        if args.keep_staging:
            print(f"[*] Kept staging directory: {tmp_dir}")
        else:
            shutil.rmtree(tmp_dir)
            print("[*] Removed temporary staging directory")

        if generated_archives:
            print("[*] Step8 package complete")
        else:
            print("[*] Step8 dry-run complete")
        return 0

    except Exception as exc:
        print(f"[!] Packaging failed: {exc}", file=sys.stderr)
        if args.keep_staging:
            print(f"[!] Staging kept for debugging: {tmp_dir}", file=sys.stderr)
        else:
            shutil.rmtree(tmp_dir, ignore_errors=True)
        return 1


if __name__ == "__main__":
    cli_args = parse_args()
    try:
        sys.exit(package_db(cli_args))
    except ValueError as exc:
        print(f"[!] {exc}", file=sys.stderr)
        sys.exit(2)
