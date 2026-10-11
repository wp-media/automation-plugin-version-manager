#!/usr/bin/env python3
"""Zip an E2E fixture plugin the way WordPress expects: one top-level folder named after the slug.

Usage: fixture_zip.py FIXTURES_DIR SLUG OUT_ZIP [--variant NAME] [--version X.Y.Z]

The zip holds FIXTURES_DIR/plugins/SLUG/ under SLUG/. With --variant, the files of
FIXTURES_DIR/variants/SLUG/NAME/ are added on top (a marker file, like BackWPup pro-de vs
pro-en). With --version, the "Version:" header of the main file(s) is rewritten, to build
an upgrade of the same plugin. Hidden files (any path part starting with ".") are left out
and symlinks are refused. Entries are sorted, timestamped 1980-01-01 and stored as regular
0644 files, so the same inputs give the same bytes on the same Python/zlib build.
"""

from __future__ import annotations

import argparse
import pathlib
import re
import stat
import sys
import zipfile

FIXED_TIME = (1980, 1, 1, 0, 0, 0)
# One header line only: never crosses into the next line, keeps a CRLF line end.
HEADER_VERSION = re.compile(rb"^( \* Version:[ \t]*)[^\r\n]*", re.MULTILINE)
VALID_VERSION = re.compile(r"^[0-9A-Za-z][0-9A-Za-z.+-]{0,31}$")
REGULAR_0644 = (stat.S_IFREG | 0o644) << 16


class FixtureError(Exception):
    """A fixture tree that cannot be zipped as asked."""


def collect(slug_dir: pathlib.Path, overlay: pathlib.Path | None) -> dict[str, bytes]:
    """Map each relative path to its bytes: the plugin tree, then the overlay on top.

    Args:
        slug_dir: the fixture plugin's folder.
        overlay: an optional folder whose files are added or replaced.

    Returns:
        Relative POSIX path -> file content.

    Raises:
        FixtureError: a symlink inside the tree.
    """
    files: dict[str, bytes] = {}
    for root in (slug_dir, overlay):
        if root is None:
            continue
        for path in sorted(root.rglob("*")):
            relative = path.relative_to(root)
            if any(part.startswith(".") for part in relative.parts):
                continue
            if path.is_symlink():
                raise FixtureError(f"symlinks are not allowed in fixtures: {path}")
            if path.is_file():
                files[relative.as_posix()] = path.read_bytes()
    return files


def set_version(files: dict[str, bytes], version: str) -> int:
    """Rewrite the Version header of every top-level PHP file that has a Plugin Name header.

    Args:
        files: the plugin's files, edited in place.
        version: the new version string.

    Returns:
        How many files were changed.
    """
    changed = 0
    replacement = version.encode()
    for name, content in files.items():
        if "/" in name or not name.endswith(".php") or b"Plugin Name:" not in content[:8192]:
            continue
        # A function replacement: the version is data, never a regex template.
        files[name], count = HEADER_VERSION.subn(lambda match: match.group(1) + replacement, content, count=1)
        changed += count
    return changed


def write_zip(out: pathlib.Path, slug: str, files: dict[str, bytes]) -> None:
    """Write the files under SLUG/ into a deterministic deflated zip.

    Args:
        out: the zip to create (parent folders are created).
        slug: the top-level folder name.
        files: relative path -> content.
    """
    out.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as archive:
        for name in sorted(files):
            info = zipfile.ZipInfo(f"{slug}/{name}", date_time=FIXED_TIME)
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = REGULAR_0644
            archive.writestr(info, files[name])


def main() -> int:
    """Parse the arguments and build the zip.

    Returns:
        The process exit code: 0 on success, 1 on bad input.
    """
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("fixtures", type=pathlib.Path)
    parser.add_argument("slug")
    parser.add_argument("out", type=pathlib.Path)
    parser.add_argument("--variant")
    parser.add_argument("--version")
    args = parser.parse_args()

    slug_dir = args.fixtures / "plugins" / args.slug
    if not slug_dir.is_dir():
        print(f"fixture_zip: no fixture plugin '{args.slug}' in {slug_dir.parent}", file=sys.stderr)
        return 1
    overlay = None
    if args.variant:
        overlay = args.fixtures / "variants" / args.slug / args.variant
        if not overlay.is_dir():
            print(f"fixture_zip: no variant '{args.variant}' for '{args.slug}'", file=sys.stderr)
            return 1

    if args.version and not VALID_VERSION.match(args.version):
        print(f"fixture_zip: invalid version '{args.version}'", file=sys.stderr)
        return 1
    try:
        files = collect(slug_dir, overlay)
    except FixtureError as error:
        print(f"fixture_zip: {error}", file=sys.stderr)
        return 1
    if args.version and set_version(files, args.version) == 0:
        print(f"fixture_zip: no Version header to rewrite in '{args.slug}'", file=sys.stderr)
        return 1
    write_zip(args.out, args.slug, files)
    return 0


if __name__ == "__main__":
    sys.exit(main())
