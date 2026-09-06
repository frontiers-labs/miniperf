#!/usr/bin/env python3
"""Fail when a rendered mdbook page links to a local file that does not exist."""
import re
import sys
from pathlib import Path
from urllib.parse import unquote, urlsplit

HREF = re.compile(r'(?:href|src)="([^"]+)"')


def main(book: Path) -> int:
    broken = []
    for page in book.rglob("*.html"):
        for ref in HREF.findall(page.read_text(encoding="utf-8")):
            parts = urlsplit(ref)
            if parts.scheme or ref.startswith(("#", "/")) or not parts.path:
                continue
            target = (page.parent / unquote(parts.path)).resolve()
            if not target.exists():
                broken.append(f"{page.relative_to(book)}: {ref}")
    for line in broken:
        print(line)
    return 1 if broken else 0


if __name__ == "__main__":
    sys.exit(main(Path(sys.argv[1] if len(sys.argv) > 1 else "book")))
