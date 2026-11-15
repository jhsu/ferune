import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import release


class ChangesetTests(unittest.TestCase):
    def test_parses_changeset_and_multiline_summary(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "change.md"
            path.write_text(
                '---\n"ferune": patch\n---\n\nFix the preview.\nKeep line two.\n',
                encoding="utf-8",
            )
            change = release.parse_changeset(path)
            self.assertEqual(change.bump, "patch")
            self.assertEqual(change.summary, "Fix the preview.\nKeep line two.")

    def test_rejects_other_packages_and_invalid_bumps(self):
        for header in ['"website": patch', '"ferune": minorly']:
            with self.subTest(header=header), tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / "change.md"
                path.write_text(f"---\n{header}\n---\n\nSummary\n", encoding="utf-8")
                with self.assertRaises(release.ChangesetError):
                    release.parse_changeset(path)

    def test_uses_the_highest_bump_for_all_changesets(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / ".changeset").mkdir()
            (root / ".changeset" / "patch.md").write_text(
                '---\n"ferune": patch\n---\n\nA fix.\n', encoding="utf-8"
            )
            (root / ".changeset" / "minor.md").write_text(
                '---\n"ferune": minor\n---\n\nA feature.\n', encoding="utf-8"
            )
            (root / "Cargo.toml").write_text(
                '[package]\nname = "ferune"\nversion = "0.1.0"\n',
                encoding="utf-8",
            )
            (root / "Cargo.lock").write_text(
                '[[package]]\nname = "ferune"\nversion = "0.1.0"\n',
                encoding="utf-8",
            )
            result = release.apply_release(root)
            self.assertEqual(result.version, "0.2.0")
            self.assertEqual(result.notes, "- A feature.\n- A fix.")
            self.assertNotIn("0.1.0", (root / "Cargo.toml").read_text())
            self.assertIn('version = "0.2.0"', (root / "Cargo.lock").read_text())
            self.assertFalse((root / ".changeset" / "patch.md").exists())
            self.assertTrue((root / "CHANGELOG.md").exists())


if __name__ == "__main__":
    unittest.main()
