#!/usr/bin/env python3
"""Tests for the dependency-free release helper."""

from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("release.py")


class ReleaseScriptTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp_dir = tempfile.TemporaryDirectory()
        self.root = Path(self.temp_dir.name)
        (self.root / "crates/standx-cli").mkdir(parents=True)

    def tearDown(self) -> None:
        self.temp_dir.cleanup()

    def write_fixture(
        self,
        *,
        version: str = "1.3.2",
        unreleased: str = "### Fixed\n- Legacy configs load again.\n",
    ) -> None:
        (self.root / "crates/standx-cli/Cargo.toml").write_text(
            f'[package]\nname = "standx-cli"\nversion = "{version}"\n',
            encoding="utf-8",
        )
        (self.root / "Cargo.lock").write_text(
            '[[package]]\n'
            'name = "standx-maker"\n'
            'version = "0.1.0"\n\n'
            '[[package]]\n'
            'name = "standx-cli"\n'
            f'version = "{version}"\n'
            'dependencies = []\n',
            encoding="utf-8",
        )
        (self.root / "CHANGELOG.md").write_text(
            "# Changelog\n\n"
            "## [Unreleased]\n\n"
            f"{unreleased}"
            "\n## [1.3.2] - 2026-07-31\n\n"
            "Previous release.\n",
            encoding="utf-8",
        )

    def run_script(self, *args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, str(SCRIPT), "--root", str(self.root), *args],
            check=check,
            capture_output=True,
            text=True,
        )

    def test_prepare_patch_updates_manifest_lockfile_and_changelog(self) -> None:
        self.write_fixture()
        paths = (
            self.root / "crates/standx-cli/Cargo.toml",
            self.root / "Cargo.lock",
            self.root / "CHANGELOG.md",
        )
        modes_before = {path: path.stat().st_mode & 0o777 for path in paths}

        result = self.run_script("prepare", "patch", "--date", "2026-08-01")

        self.assertEqual(result.stdout.strip(), "prepared v1.3.3")
        self.assertIn(
            'version = "1.3.3"',
            (self.root / "crates/standx-cli/Cargo.toml").read_text(encoding="utf-8"),
        )
        self.assertIn(
            'name = "standx-cli"\nversion = "1.3.3"',
            (self.root / "Cargo.lock").read_text(encoding="utf-8"),
        )
        changelog = (self.root / "CHANGELOG.md").read_text(encoding="utf-8")
        self.assertIn(
            "## [Unreleased]\n\n## [1.3.3] - 2026-08-01\n\n"
            "### Fixed\n- Legacy configs load again.\n",
            changelog,
        )
        for path in paths:
            self.assertEqual(path.stat().st_mode & 0o777, modes_before[path])

        notes = self.run_script("notes", "--version", "1.3.3")
        self.assertEqual(notes.stdout, "### Fixed\n- Legacy configs load again.\n")

    def test_prepare_rejects_empty_unreleased_section_without_partial_writes(self) -> None:
        self.write_fixture(unreleased="")
        before = {
            path: path.read_text(encoding="utf-8")
            for path in (
                self.root / "crates/standx-cli/Cargo.toml",
                self.root / "Cargo.lock",
                self.root / "CHANGELOG.md",
            )
        }

        result = self.run_script(
            "prepare",
            "patch",
            "--date",
            "2026-08-01",
            check=False,
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Unreleased section is empty", result.stderr)
        for path, content in before.items():
            self.assertEqual(path.read_text(encoding="utf-8"), content)

    def test_verify_checks_tag_and_built_binary_versions(self) -> None:
        self.write_fixture()
        binary = self.root / "standx"
        binary.write_text("#!/bin/sh\nprintf 'standx 1.3.2\\n'\n", encoding="utf-8")
        binary.chmod(0o755)

        self.run_script("verify", "--tag", "v1.3.2", "--binary", str(binary))
        self.run_script("verify", "--tag", "v1.3.2-rc.1")

        wrong_tag = self.run_script("verify", "--tag", "v1.3.3", check=False)
        self.assertNotEqual(wrong_tag.returncode, 0)
        self.assertIn("tag v1.3.3 does not match Cargo version 1.3.2", wrong_tag.stderr)

        binary.write_text("#!/bin/sh\nprintf 'standx 1.3.1\\n'\n", encoding="utf-8")
        binary.chmod(0o755)
        wrong_binary = self.run_script(
            "verify",
            "--tag",
            "v1.3.2",
            "--binary",
            str(binary),
            check=False,
        )
        self.assertNotEqual(wrong_binary.returncode, 0)
        self.assertIn(
            "binary reports 'standx 1.3.1', expected 'standx 1.3.2'",
            wrong_binary.stderr,
        )

    def test_verify_rejects_manifest_lockfile_drift(self) -> None:
        self.write_fixture()
        lockfile = self.root / "Cargo.lock"
        lockfile.write_text(
            lockfile.read_text(encoding="utf-8").replace(
                'name = "standx-cli"\nversion = "1.3.2"',
                'name = "standx-cli"\nversion = "1.3.1"',
            ),
            encoding="utf-8",
        )

        result = self.run_script("verify", check=False)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            "Cargo.lock version 1.3.1 does not match Cargo.toml version 1.3.2",
            result.stderr,
        )


if __name__ == "__main__":
    unittest.main()
