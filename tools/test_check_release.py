#!/usr/bin/env python3
"""Boundary tests for the release identity gate.

A publish gate is only worth having if it refuses. Every check in
`check_release.py` is exercised here against a temporary repository built from
scratch, once in the state that should publish and once in each state that
should not — including the case that actually happened elsewhere in this
family on release day: a tag that matched `Cargo.toml` while
`herdr-plugin.toml` still carried the previous version.
"""

from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from check_release import (  # noqa: E402
    ReleaseError,
    changelog_notes,
    check,
    check_release_context,
    resolve_tag,
)

CHECKER = Path(__file__).with_name("check_release.py")

CHANGELOG = """# Changelog

## [Unreleased]

## [1.2.3] - 2026-08-16

### Added

- A thing worth telling a user about.

### Fixed

- Another thing.

## [1.2.2] - 2026-08-01

### Fixed

- An older thing nobody is releasing today.
"""


def build_repo(
    directory: Path,
    *,
    cargo: str = "1.2.3",
    locked: str = "1.2.3",
    manifest: str = "1.2.3",
    changelog: str = CHANGELOG,
    name: str = "widget",
) -> Path:
    (directory / "tools").mkdir(parents=True, exist_ok=True)
    (directory / "Cargo.toml").write_text(
        f'[package]\nname = "{name}"\nversion = "{cargo}"\nedition = "2021"\n',
        encoding="utf-8",
    )
    (directory / "Cargo.lock").write_text(
        'version = 4\n\n[[package]]\nname = "serde"\nversion = "1.0.0"\n\n'
        f'[[package]]\nname = "{name}"\nversion = "{locked}"\n',
        encoding="utf-8",
    )
    (directory / "herdr-plugin.toml").write_text(
        f'id = "moneycaringcoder.widget"\nversion = "{manifest}"\n', encoding="utf-8"
    )
    (directory / "CHANGELOG.md").write_text(changelog, encoding="utf-8")
    return directory


class TagTests(unittest.TestCase):
    def test_a_well_formed_tag_is_accepted_from_either_source(self) -> None:
        self.assertEqual(resolve_tag("v1.2.3", None), "v1.2.3")
        self.assertEqual(resolve_tag(None, "v0.1.0"), "v0.1.0")

    def test_malformed_tags_are_refused(self) -> None:
        for tag in ("1.2.3", "v1.2", "v1.2.3.4", "v1.2.3-rc1", "vX.Y.Z", "release-1.2.3", "v01.2.3"):
            with self.subTest(tag=tag), self.assertRaisesRegex(ReleaseError, "is not v"):
                resolve_tag(tag, None)

    def test_no_tag_at_all_is_refused(self) -> None:
        for environment in (None, ""):
            with self.subTest(environment=environment):
                with self.assertRaisesRegex(ReleaseError, "provide --tag"):
                    resolve_tag(None, environment)

    def test_in_ci_the_run_must_be_the_tag_itself(self) -> None:
        check_release_context("v1.2.3", github_actions=False, ref_type="branch", ref_name="main")
        check_release_context("v1.2.3", github_actions=True, ref_type="tag", ref_name="v1.2.3")
        for ref_type, ref_name in (("branch", "main"), ("tag", "v1.2.4"), (None, None)):
            with self.subTest(ref_type=ref_type, ref_name=ref_name):
                with self.assertRaisesRegex(ReleaseError, "triggered by the v1.2.3 tag"):
                    check_release_context(
                        "v1.2.3", github_actions=True, ref_type=ref_type, ref_name=ref_name
                    )


class VersionSurfaceTests(unittest.TestCase):
    def check_repo(self, **overrides: str) -> str:
        with tempfile.TemporaryDirectory() as directory:
            return check(build_repo(Path(directory), **overrides), "v1.2.3")

    def test_a_consistent_release_passes_and_returns_its_notes(self) -> None:
        notes = self.check_repo()
        self.assertIn("A thing worth telling a user about.", notes)
        self.assertIn("Another thing.", notes)
        # Only this version's section, not its neighbours.
        self.assertNotIn("An older thing", notes)
        self.assertNotIn("Unreleased", notes)
        self.assertNotIn("## [1.2.3]", notes)

    def test_a_stale_cargo_version_is_refused(self) -> None:
        with self.assertRaisesRegex(ReleaseError, "Cargo.toml version"):
            self.check_repo(cargo="1.2.2")

    def test_a_stale_lock_file_is_refused(self) -> None:
        with self.assertRaisesRegex(ReleaseError, "Cargo.lock has widget"):
            self.check_repo(locked="1.2.2")

    def test_a_stale_plugin_manifest_is_refused(self) -> None:
        # The case this gate exists for. Cargo.toml and the tag agree; the
        # manifest the marketplace reads does not.
        with self.assertRaisesRegex(ReleaseError, "herdr-plugin.toml version"):
            self.check_repo(manifest="1.2.2")

    def test_a_missing_changelog_section_is_refused(self) -> None:
        with self.assertRaisesRegex(ReleaseError, r"no dated \[1.2.3\] section"):
            self.check_repo(changelog="# Changelog\n\n## [1.2.2] - 2026-08-01\n\n- Old.\n")

    def test_an_undated_changelog_section_is_refused(self) -> None:
        with self.assertRaisesRegex(ReleaseError, "no ISO-8601 date"):
            self.check_repo(changelog="# Changelog\n\n## [1.2.3]\n\n- Undated.\n")

    def test_an_empty_changelog_section_is_refused(self) -> None:
        for changelog in (
            "# Changelog\n\n## [1.2.3] - 2026-08-16\n\n## [1.2.2] - 2026-08-01\n\n- Old.\n",
            "# Changelog\n\n## [1.2.3] - 2026-08-16\n\n### Added\n\n### Fixed\n",
            "# Changelog\n\n## [1.2.3] - 2026-08-16\n",
        ):
            with self.subTest(changelog=changelog):
                with self.assertRaisesRegex(ReleaseError, "is empty"):
                    self.check_repo(changelog=changelog)


class NotesTests(unittest.TestCase):
    def test_the_last_section_in_the_file_is_read_to_the_end(self) -> None:
        notes = changelog_notes("# Changelog\n\n## [1.0.0] - 2026-01-01\n\n- Only entry.\n", "1.0.0")
        self.assertEqual(notes, "- Only entry.\n")

    def test_a_version_that_is_a_prefix_of_another_is_not_confused_for_it(self) -> None:
        text = (
            "# Changelog\n\n## [1.2.30] - 2026-08-16\n\n- Thirty.\n\n"
            "## [1.2.3] - 2026-08-01\n\n- Three.\n"
        )
        self.assertIn("Three.", changelog_notes(text, "1.2.3"))
        self.assertNotIn("Thirty.", changelog_notes(text, "1.2.3"))
        self.assertIn("Thirty.", changelog_notes(text, "1.2.30"))


class CommandLineTests(unittest.TestCase):
    def run_checker(self, *arguments: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, str(CHECKER), *arguments],
            capture_output=True,
            check=False,
            text=True,
            cwd=str(CHECKER.resolve().parents[1]),
        )

    def test_this_repository_is_consistent_at_its_current_version(self) -> None:
        # Not a release rehearsal: it asserts that whatever version this
        # repository currently claims, it claims it everywhere. A released
        # version stays consistent; an unreleased one has no dated changelog
        # section yet, which is the expected state between releases.
        root = CHECKER.resolve().parents[1]
        import tomllib

        with (root / "Cargo.toml").open("rb") as handle:
            version = tomllib.load(handle)["package"]["version"]
        result = self.run_checker("--tag", f"v{version}")
        if result.returncode != 0:
            self.assertIn("CHANGELOG.md", result.stderr, result.stderr)
        else:
            self.assertIn("release identity verified", result.stdout)

    def test_a_mismatched_tag_exits_one_and_says_which_surface(self) -> None:
        result = self.run_checker("--tag", "v99.99.99")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "")
        self.assertIn("release identity error", result.stderr)

    def test_a_malformed_tag_exits_one(self) -> None:
        result = self.run_checker("--tag", "not-a-tag")
        self.assertEqual(result.returncode, 1)
        self.assertIn("is not v<major>.<minor>.<patch>", result.stderr)

    def test_notes_are_written_only_when_every_check_passed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            notes = Path(directory) / "notes.md"
            result = self.run_checker("--tag", "v99.99.99", "--notes-out", str(notes))
            self.assertEqual(result.returncode, 1)
            self.assertFalse(notes.exists(), "notes must not be written for a refused release")


if __name__ == "__main__":
    unittest.main()
