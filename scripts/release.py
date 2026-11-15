#!/usr/bin/env python3
"""Apply Rust-native Changesets entries, commit the version, and create its tag."""

from __future__ import annotations

import datetime as dt
import os
import re
import subprocess
import sys
import tempfile
import tomllib
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CHANGESET_DIR = ROOT / ".changeset"
CHANGELOG = ROOT / "CHANGELOG.md"
PACKAGE_NAME = "ferune"
LEVELS = {"patch": 0, "minor": 1, "major": 2}
FRONTMATTER_LINE = re.compile(r"""^\s*["']?([A-Za-z0-9_.@/-]+)["']?\s*:\s*(patch|minor|major)\s*$""")


class ChangesetError(RuntimeError):
    pass


@dataclass(frozen=True)
class Changeset:
    path: Path
    bump: str
    summary: str


@dataclass(frozen=True)
class Release:
    version: str
    notes: str
    consumed: tuple[Path, ...]


def parse_changeset(path: Path) -> Changeset:
    text = path.read_text(encoding="utf-8")
    parts = text.split("---", 2)
    if len(parts) != 3 or parts[0].strip():
        raise ChangesetError(f"{path.name}: expected YAML frontmatter bounded by ---")

    bumps: list[str] = []
    for line in parts[1].splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        match = FRONTMATTER_LINE.fullmatch(line)
        if not match:
            raise ChangesetError(f"{path.name}: invalid changeset line: {line!r}")
        package, bump = match.groups()
        if package != PACKAGE_NAME:
            raise ChangesetError(f"{path.name}: unknown package {package!r}")
        bumps.append(bump)
    if not bumps:
        raise ChangesetError(f"{path.name}: no {PACKAGE_NAME} bump was specified")

    summary = parts[2].strip()
    if not summary:
        raise ChangesetError(f"{path.name}: add a short change summary")
    return Changeset(path, max(bumps, key=LEVELS.__getitem__), summary)


def bump_version(version: str, bump: str) -> str:
    match = re.fullmatch(r"(\d+)\.(\d+)\.(\d+)", version)
    if not match:
        raise ChangesetError(f"expected a stable MAJOR.MINOR.PATCH version, got {version!r}")
    major, minor, patch = map(int, match.groups())
    if bump == "major":
        return f"{major + 1}.0.0"
    if bump == "minor":
        return f"{major}.{minor + 1}.0"
    if bump == "patch":
        return f"{major}.{minor}.{patch + 1}"
    raise ChangesetError(f"unknown bump level {bump!r}")


def render_notes(changesets: list[Changeset]) -> str:
    bullets: list[str] = []
    for changeset in changesets:
        lines = changeset.summary.splitlines()
        bullets.append(f"- {lines[0]}")
        bullets.extend(f"  {line}" for line in lines[1:])
    return "\n".join(bullets)


def set_package_version(text: str, version: str) -> str:
    match = re.search(r"(?ms)^\[package\]\s*$(.*?)(?=^\[|\Z)", text)
    if not match:
        raise ChangesetError("Cargo.toml has no [package] section")
    section = match.group(0)
    bumped, count = re.subn(
        r'(?m)^version\s*=\s*"[^"]+"\s*$',
        f'version = "{version}"',
        section,
        count=1,
    )
    if count != 1:
        raise ChangesetError("Cargo.toml [package] has no simple version = \"…\" field")
    return text[: match.start()] + bumped + text[match.end() :]


def set_lockfile_version(text: str, version: str) -> str:
    pattern = re.compile(
        r'(?ms)(^\[\[package\]\]\nname = "ferune"\nversion = ")[^"]+(")'
    )
    updated, count = pattern.subn(rf"\g<1>{version}\g<2>", text, count=1)
    if count != 1:
        raise ChangesetError("Cargo.lock has no ferune package entry; regenerate it first")
    return updated


def apply_release(root: Path) -> Release | None:
    changesets = sorted(
        path for path in (root / ".changeset").glob("*.md") if path.name != "README.md"
    )
    if not changesets:
        return None
    parsed = [parse_changeset(path) for path in changesets]
    bump = max((change.bump for change in parsed), key=LEVELS.__getitem__)

    cargo_path = root / "Cargo.toml"
    cargo = tomllib.loads(cargo_path.read_text(encoding="utf-8"))
    old_version = cargo["package"]["version"]
    version = bump_version(old_version, bump)
    cargo_path.write_text(
        set_package_version(cargo_path.read_text(encoding="utf-8"), version),
        encoding="utf-8",
    )

    lock_path = root / "Cargo.lock"
    lock_path.write_text(
        set_lockfile_version(lock_path.read_text(encoding="utf-8"), version),
        encoding="utf-8",
    )

    notes = render_notes(parsed)
    date = dt.date.today().isoformat()
    changelog = root / "CHANGELOG.md"
    current = changelog.read_text(encoding="utf-8") if changelog.exists() else "# Changelog\n"
    heading = f"## {version} - {date}\n\n{notes}\n"
    if current.startswith("# Changelog"):
        prefix, rest = current.split("\n", 1)
        current = f"{prefix}\n\n{heading}\n{rest.lstrip()}"
    else:
        current = f"# Changelog\n\n{heading}\n{current}"
    changelog.write_text(current, encoding="utf-8")
    for change in parsed:
        change.path.unlink()
    return Release(version, notes, tuple(changesets))


def write_output(released: bool, version: str = "", notes_path: str = "") -> None:
    output = os.environ.get("GITHUB_OUTPUT")
    if not output:
        return
    with open(output, "a", encoding="utf-8") as stream:
        stream.write(f"released={'true' if released else 'false'}\n")
        if released:
            stream.write(f"version={version}\n")
            stream.write(f"notes={notes_path}\n")


def main() -> int:
    try:
        release = apply_release(ROOT)
        if release is None:
            print("No Changesets found; nothing to release.")
            write_output(False)
            return 0

        notes_path = Path(os.environ.get("RUNNER_TEMP", tempfile.gettempdir()))
        notes_path = notes_path / "ferune-release-notes.md"
        notes_path.write_text(release.notes + "\n", encoding="utf-8")
        tag = f"v{release.version}"
        subprocess.run(
            ["git", "add", "Cargo.toml", "Cargo.lock", "CHANGELOG.md", ".changeset"],
            cwd=ROOT,
            check=True,
        )
        subprocess.run(
            ["git", "config", "user.name", "github-actions[bot]"],
            cwd=ROOT,
            check=True,
        )
        subprocess.run(
            [
                "git",
                "config",
                "user.email",
                "41898282+github-actions[bot]@users.noreply.github.com",
            ],
            cwd=ROOT,
            check=True,
        )
        subprocess.run(
            ["git", "commit", "-m", f"chore: release {tag} [skip ci]"],
            cwd=ROOT,
            check=True,
        )
        subprocess.run(["git", "tag", "-a", tag, "-m", tag], cwd=ROOT, check=True)
        write_output(True, release.version, str(notes_path))
        print(f"Prepared release {tag}")
        return 0
    except (ChangesetError, KeyError, OSError, subprocess.CalledProcessError) as error:
        print(f"release failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
