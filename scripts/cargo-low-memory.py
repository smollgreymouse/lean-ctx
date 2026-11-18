#!/usr/bin/env python3
"""Run the measured, opt-in low-memory Cargo build/check profile."""

from __future__ import annotations

import argparse
import hashlib
import os
import subprocess
import sys
import tempfile
from pathlib import Path


MEMORY_PROFILE = {
    "CARGO_BUILD_JOBS": "1",
    "CARGO_INCREMENTAL": "1",
    "CARGO_PROFILE_DEV_DEBUG": "0",
    "CARGO_PROFILE_DEV_CODEGEN_UNITS": "1",
}
TEMP_FILESYSTEMS = {"devtmpfs", "ramfs", "tmpfs"}


def _is_relative_to(path: Path, parent: Path) -> bool:
    try:
        path.relative_to(parent)
        return True
    except ValueError:
        return False


def _resolved(path: str | Path, *, relative_to: Path) -> Path:
    value = Path(path).expanduser()
    if not value.is_absolute():
        value = relative_to / value
    return value.resolve(strict=False)


def _linux_filesystem_type(path: Path) -> str | None:
    if not sys.platform.startswith("linux"):
        return None
    try:
        lines = Path("/proc/self/mountinfo").read_text(encoding="utf-8").splitlines()
    except OSError:
        return None

    def unescape(value: str) -> str:
        return (
            value.replace("\\040", " ")
            .replace("\\011", "\t")
            .replace("\\012", "\n")
            .replace("\\134", "\\")
        )

    matches: list[tuple[int, str]] = []
    for line in lines:
        before, separator, after = line.partition(" - ")
        left = before.split()
        right = after.split()
        if not separator or len(left) < 5 or not right:
            continue
        mount_point = Path(unescape(left[4]))
        if _is_relative_to(path, mount_point):
            matches.append((len(mount_point.parts), right[0]))
    return max(matches)[1] if matches else None


def validate_target_dir(target_dir: Path, *, env: dict[str, str]) -> None:
    if ".git" in {part.lower() for part in target_dir.parts}:
        raise ValueError(f"Cargo target directory must be outside Git metadata: {target_dir}")

    temporary_roots = {Path(tempfile.gettempdir()).resolve(strict=False)}
    for name in ("TMPDIR", "TMP", "TEMP"):
        raw = env.get(name)
        if raw:
            temporary_roots.add(_resolved(raw, relative_to=Path.cwd()))
    temporary_roots.update(
        Path(root).resolve(strict=False) for root in ("/tmp", "/var/tmp", "/dev/shm", "/run")
    )
    if any(_is_relative_to(target_dir, root) for root in temporary_roots):
        raise ValueError(f"Cargo target directory must not be in a temporary directory: {target_dir}")

    filesystem_type = _linux_filesystem_type(target_dir)
    if filesystem_type in TEMP_FILESYSTEMS:
        raise ValueError(
            f"Cargo target directory is on {filesystem_type}; choose a persistent disk-backed directory: {target_dir}"
        )


def target_dir_for(rust_root: Path, *, env: dict[str, str]) -> Path:
    override = env.get("CARGO_TARGET_DIR")
    if override:
        target_dir = _resolved(override, relative_to=rust_root)
    else:
        raw_cache_home = env.get("XDG_CACHE_HOME")
        if raw_cache_home:
            cache_home = Path(raw_cache_home).expanduser()
            if not cache_home.is_absolute():
                raise ValueError("XDG_CACHE_HOME must be an absolute path")
        else:
            cache_home = Path.home() / ".cache"
        canonical_root = rust_root.resolve(strict=True)
        checkout_key = hashlib.sha256(os.fsencode(canonical_root)).hexdigest()[:24]
        target_dir = (cache_home / "leanctx" / "low-memory" / checkout_key).resolve(strict=False)

    validate_target_dir(target_dir, env=env)
    return target_dir


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("build", "check"))
    args = parser.parse_args()

    repo_root = Path(__file__).resolve().parents[1]
    rust_root = repo_root / "rust"
    if not (rust_root / "Cargo.toml").is_file():
        parser.error(f"Rust workspace Cargo.toml not found: {rust_root / 'Cargo.toml'}")

    env = os.environ.copy()
    try:
        target_dir = target_dir_for(rust_root, env=env)
    except ValueError as exc:
        parser.error(str(exc))

    env["CARGO_TARGET_DIR"] = str(target_dir)
    env.update(MEMORY_PROFILE)

    command = [
        "cargo",
        "--config",
        "profile.dev.package.lean-ctx.opt-level=0",
        args.action,
    ]
    if args.action == "build":
        command.extend(("--bin", "lean-ctx"))
    else:
        command.extend(("--lib", "--tests"))
    command.extend(("-j", "1"))

    print(f"Cargo target directory: {target_dir}", file=sys.stderr)
    return subprocess.run(command, cwd=rust_root, env=env, check=False).returncode


if __name__ == "__main__":
    raise SystemExit(main())
