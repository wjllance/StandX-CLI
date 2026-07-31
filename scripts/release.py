#!/usr/bin/env python3
"""Prepare and verify StandX CLI releases without third-party dependencies."""

from __future__ import annotations

import argparse
import datetime as dt
import os
import re
import stat
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Optional, Sequence, Tuple


DEFAULT_ROOT = Path(__file__).resolve().parents[1]
SEMVER_RE = re.compile(r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$")
TAG_RE = re.compile(
    r"^v((?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*))"
    r"(?:-[0-9A-Za-z][0-9A-Za-z.-]*)?$"
)
VERSION_LINE_RE = re.compile(r'^version = "([^"]+)"$', re.MULTILINE)
PACKAGE_BLOCK_RE = re.compile(
    r"^\[\[package\]\]\n.*?(?=^\[\[package\]\]|\Z)",
    re.MULTILINE | re.DOTALL,
)
CHANGELOG_SECTION_RE = re.compile(r"^## \[([^\]]+)\](?: - [^\n]+)?$", re.MULTILINE)


class ReleaseError(Exception):
    """A release invariant was not satisfied."""


def parse_semver(value: str) -> Tuple[int, int, int]:
    match = SEMVER_RE.fullmatch(value)
    if match is None:
        raise ReleaseError(
            f"version {value!r} is not a stable X.Y.Z semantic version"
        )
    major, minor, patch = match.groups()
    return int(major), int(minor), int(patch)


def bump_semver(version: str, part: str) -> str:
    major, minor, patch = parse_semver(version)
    if part == "major":
        return f"{major + 1}.0.0"
    if part == "minor":
        return f"{major}.{minor + 1}.0"
    if part == "patch":
        return f"{major}.{minor}.{patch + 1}"
    raise ReleaseError(f"unsupported version bump {part!r}")


def read_text(path: Path) -> str:
    try:
        return path.read_text(encoding="utf-8")
    except OSError as error:
        raise ReleaseError(f"failed to read {path}: {error}") from error


def manifest_version(manifest: str) -> str:
    matches = VERSION_LINE_RE.findall(manifest)
    if len(matches) != 1:
        raise ReleaseError(
            "crates/standx-cli/Cargo.toml must contain exactly one package version"
        )
    parse_semver(matches[0])
    return matches[0]


def standx_lock_version(lockfile: str) -> str:
    matching_blocks = []
    for block_match in PACKAGE_BLOCK_RE.finditer(lockfile):
        block = block_match.group(0)
        if re.search(r'^name = "standx-cli"$', block, re.MULTILINE):
            matching_blocks.append(block)
    if len(matching_blocks) != 1:
        raise ReleaseError(
            "Cargo.lock must contain exactly one [[package]] block for standx-cli"
        )
    version_matches = VERSION_LINE_RE.findall(matching_blocks[0])
    if len(version_matches) != 1:
        raise ReleaseError(
            "the standx-cli Cargo.lock block must contain exactly one version"
        )
    parse_semver(version_matches[0])
    return version_matches[0]


def replace_manifest_version(manifest: str, old: str, new: str) -> str:
    old_line = f'version = "{old}"'
    new_line = f'version = "{new}"'
    if manifest.count(old_line) != 1:
        raise ReleaseError(
            f"expected exactly one {old_line!r} in crates/standx-cli/Cargo.toml"
        )
    return manifest.replace(old_line, new_line, 1)


def replace_lock_version(lockfile: str, old: str, new: str) -> str:
    blocks = list(PACKAGE_BLOCK_RE.finditer(lockfile))
    matching = [
        block
        for block in blocks
        if re.search(r'^name = "standx-cli"$', block.group(0), re.MULTILINE)
    ]
    if len(matching) != 1:
        raise ReleaseError(
            "Cargo.lock must contain exactly one [[package]] block for standx-cli"
        )
    block_match = matching[0]
    block = block_match.group(0)
    old_line = f'version = "{old}"'
    if block.count(old_line) != 1:
        raise ReleaseError(
            f"expected the standx-cli Cargo.lock block to contain {old_line!r}"
        )
    updated_block = block.replace(old_line, f'version = "{new}"', 1)
    return (
        lockfile[: block_match.start()]
        + updated_block
        + lockfile[block_match.end() :]
    )


def changelog_section(changelog: str, label: str) -> str:
    matches = [
        match
        for match in CHANGELOG_SECTION_RE.finditer(changelog)
        if match.group(1) == label
    ]
    if len(matches) != 1:
        raise ReleaseError(
            f"CHANGELOG.md must contain exactly one '## [{label}]' section"
        )
    match = matches[0]
    next_match = CHANGELOG_SECTION_RE.search(changelog, match.end())
    end = next_match.start() if next_match is not None else len(changelog)
    return changelog[match.end() : end].strip()


def promote_unreleased(changelog: str, version: str, release_date: str) -> str:
    body = changelog_section(changelog, "Unreleased")
    if not body:
        raise ReleaseError(
            "CHANGELOG.md Unreleased section is empty; add release notes before preparing"
        )
    if any(
        match.group(1) == version
        for match in CHANGELOG_SECTION_RE.finditer(changelog)
    ):
        raise ReleaseError(f"CHANGELOG.md already contains a {version} section")
    unreleased_match = next(
        match
        for match in CHANGELOG_SECTION_RE.finditer(changelog)
        if match.group(1) == "Unreleased"
    )
    next_match = CHANGELOG_SECTION_RE.search(changelog, unreleased_match.end())
    suffix_start = next_match.start() if next_match is not None else len(changelog)
    prefix = changelog[: unreleased_match.start()]
    suffix = changelog[suffix_start:].lstrip("\n")
    result = (
        f"{prefix}## [Unreleased]\n\n"
        f"## [{version}] - {release_date}\n\n"
        f"{body}\n"
    )
    if suffix:
        result += f"\n{suffix}"
    return result


def atomic_write(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    existing_mode = stat.S_IMODE(path.stat().st_mode) if path.exists() else 0o644
    descriptor, temporary_name = tempfile.mkstemp(
        prefix=f".{path.name}.", dir=str(path.parent)
    )
    temporary_path = Path(temporary_name)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8") as handle:
            handle.write(content)
        temporary_path.chmod(existing_mode)
        os.replace(temporary_path, path)
    except Exception:
        try:
            temporary_path.unlink()
        except FileNotFoundError:
            pass
        raise


class Repository:
    def __init__(self, root: Path) -> None:
        self.root = root.resolve()
        self.manifest_path = self.root / "crates/standx-cli/Cargo.toml"
        self.lockfile_path = self.root / "Cargo.lock"
        self.changelog_path = self.root / "CHANGELOG.md"

    def read_versions(self) -> Tuple[str, str]:
        cargo_version = manifest_version(read_text(self.manifest_path))
        lock_version = standx_lock_version(read_text(self.lockfile_path))
        return cargo_version, lock_version

    def verify(
        self,
        *,
        tag: Optional[str] = None,
        binary: Optional[Path] = None,
    ) -> str:
        cargo_version, lock_version = self.read_versions()
        if lock_version != cargo_version:
            raise ReleaseError(
                f"Cargo.lock version {lock_version} does not match "
                f"Cargo.toml version {cargo_version}"
            )
        changelog = read_text(self.changelog_path)
        changelog_section(changelog, "Unreleased")
        changelog_section(changelog, cargo_version)
        if tag is not None:
            tag_match = TAG_RE.fullmatch(tag)
            if tag_match is None or tag_match.group(1) != cargo_version:
                raise ReleaseError(
                    f"tag {tag} does not match Cargo version {cargo_version}"
                )
        if binary is not None:
            try:
                result = subprocess.run(
                    [str(binary), "--version"],
                    check=True,
                    capture_output=True,
                    text=True,
                )
            except (OSError, subprocess.CalledProcessError) as error:
                raise ReleaseError(
                    f"failed to execute {binary} --version: {error}"
                ) from error
            reported = result.stdout.strip()
            expected = f"standx {cargo_version}"
            if reported != expected:
                raise ReleaseError(
                    f"binary reports {reported!r}, expected {expected!r}"
                )
        return cargo_version

    def prepare(self, bump: str, release_date: str) -> str:
        try:
            dt.date.fromisoformat(release_date)
        except ValueError as error:
            raise ReleaseError(
                f"release date {release_date!r} is not YYYY-MM-DD"
            ) from error

        manifest = read_text(self.manifest_path)
        lockfile = read_text(self.lockfile_path)
        changelog = read_text(self.changelog_path)
        current = manifest_version(manifest)
        lock_version = standx_lock_version(lockfile)
        if lock_version != current:
            raise ReleaseError(
                f"Cargo.lock version {lock_version} does not match "
                f"Cargo.toml version {current}"
            )
        new_version = bump_semver(current, bump)

        updated_manifest = replace_manifest_version(manifest, current, new_version)
        updated_lockfile = replace_lock_version(lockfile, current, new_version)
        updated_changelog = promote_unreleased(
            changelog, new_version, release_date
        )

        atomic_write(self.manifest_path, updated_manifest)
        atomic_write(self.lockfile_path, updated_lockfile)
        atomic_write(self.changelog_path, updated_changelog)
        return new_version

    def notes(self, version: str) -> str:
        parse_semver(version)
        body = changelog_section(read_text(self.changelog_path), version)
        if not body:
            raise ReleaseError(f"CHANGELOG.md section {version} is empty")
        return body


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--root",
        type=Path,
        default=DEFAULT_ROOT,
        help=argparse.SUPPRESS,
    )
    commands = parser.add_subparsers(dest="command", required=True)

    prepare = commands.add_parser(
        "prepare",
        help="promote CHANGELOG Unreleased and bump the CLI version",
    )
    prepare.add_argument("bump", choices=("patch", "minor", "major"))
    prepare.add_argument(
        "--date",
        default=dt.date.today().isoformat(),
        help="release date in YYYY-MM-DD form (default: today)",
    )

    verify = commands.add_parser(
        "verify",
        help="check Cargo, changelog, tag, and optional binary consistency",
    )
    verify.add_argument("--tag", help="expected release tag, including the v prefix")
    verify.add_argument("--binary", type=Path, help="built standx binary to inspect")

    notes = commands.add_parser(
        "notes",
        help="print one version's CHANGELOG body",
    )
    notes.add_argument("--version", help="version to print (default: Cargo version)")
    return parser


def main(argv: Optional[Sequence[str]] = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    repository = Repository(args.root)
    try:
        if args.command == "prepare":
            version = repository.prepare(args.bump, args.date)
            print(f"prepared v{version}")
        elif args.command == "verify":
            version = repository.verify(tag=args.tag, binary=args.binary)
            print(f"verified v{version}")
        elif args.command == "notes":
            version = args.version
            if version is None:
                version = repository.read_versions()[0]
            print(repository.notes(version))
        else:
            parser.error(f"unsupported command {args.command!r}")
    except ReleaseError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
