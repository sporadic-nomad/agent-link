#!/usr/bin/env python3
"""Check public Markdown links and the public/local documentation boundary."""

from pathlib import Path
import re
import subprocess
from urllib.parse import unquote, urlsplit


ROOT = Path(__file__).resolve().parent.parent


def tracked_files():
    """Only consult Git when this is the checkout root, not an extracted crate."""
    try:
        top = subprocess.run(
            ["git", "rev-parse", "--show-toplevel"], cwd=ROOT,
            capture_output=True, text=True, check=True,
        ).stdout.strip()
        if Path(top).resolve() != ROOT:
            return None
        paths = subprocess.run(
            ["git", "ls-files", "-z"], cwd=ROOT,
            capture_output=True, text=True, check=True,
        ).stdout
        return set(paths.rstrip("\0").split("\0"))
    except (FileNotFoundError, subprocess.CalledProcessError):
        return None


def main():
    errors = []
    tracked = tracked_files()
    manual = sorted((ROOT / "docs").rglob("*.md"))
    for required in ("docs/README.md", "docs/permission-validation.md"):
        if not (ROOT / required).is_file():
            errors.append(f"missing public documentation: {required}")
    if tracked is not None:
        for path in manual:
            relative = path.relative_to(ROOT).as_posix()
            if relative not in tracked:
                errors.append(f"public manual is not tracked: {relative}")
        if any(path.startswith("internal-docs/") for path in tracked):
            errors.append("internal-docs must stay local and untracked")

    # These public entry points must also work in the core crate archive.
    pages = manual + [ROOT / name for name in ("README.md", "CONTRIBUTING.md", "CHANGELOG.md")]
    for page in pages:
        for destination in re.findall(r"\]\(([^\s)]+)", page.read_text(encoding="utf-8")):
            url = urlsplit(destination)
            if url.scheme or url.netloc or not url.path:
                continue
            target = (page.parent / unquote(url.path)).resolve()
            try:
                relative = target.relative_to(ROOT).as_posix()
            except ValueError:
                errors.append(f"{page.name}: link leaves the project: {destination}")
                continue
            if relative == "internal-docs" or relative.startswith("internal-docs/"):
                errors.append(f"{page.name}: public link depends on local notes: {destination}")
            elif not target.exists():
                errors.append(f"{page.name}: broken local link: {destination}")
            elif tracked is not None and target.is_file() and relative not in tracked:
                errors.append(f"{page.name}: linked file is not tracked: {relative}")

    if errors:
        raise SystemExit("\n".join(errors))
    print(f"Public documentation: {len(manual)} pages; links and documentation boundary passed")


if __name__ == "__main__":
    main()
