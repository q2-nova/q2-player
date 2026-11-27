#!/usr/bin/env python3
"""Check the repository's Markdown documents and requirement IDs.

Every relative link in a Markdown file outside crates/vendor must reach a
tracked file, and an anchor must name a heading in the target. Links listed
in tools/doc-links-pending.txt point at documents that are not ported yet;
they are allowed until the target exists.

docs/specification.md must define each requirement ID once, in a table row
with a requirement and an acceptance, and must carry no dates, release
numbers or references to people. A table headed ID | Repository names IDs
that another repository's specification defines. Every ID of a
specification family cited in source, tests, tools or docs must be defined
there.
"""
import pathlib
import re
import subprocess
import sys
import unicodedata

ROOT = pathlib.Path(__file__).resolve().parent.parent
PENDING = ROOT / 'tools' / 'doc-links-pending.txt'
SPEC = 'docs/specification.md'
EXCLUDED = ('crates/vendor/',)
LINK = re.compile(r'(?<!!)\[[^\]]*\]\(([^)\s]+)\)')
DATE = re.compile(r'\b(19|20)\d\d-\d\d-\d\d\b|\b(Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)[a-z]* \d{1,2},? (19|20)\d\d\b')
RELEASE = re.compile(r'\bV\d+\.\d+\b')
PEOPLE = re.compile(r"(?i)\b(device owner|the owner|owner's|requested by|product feedback)\b|\b(he|she|his|her|him)\b")
ID = re.compile(r'\b([A-Z][A-Z]*(?:-[A-Z]+)*)-(\d{3})\b')
ROW_ID = re.compile(r'[A-Z][A-Z]*(?:-[A-Z]+)*-\d{3}')
CITING = ('crates/', 'docs/', 'tests/', 'tools/', 'web-demo/', 'README.md')


def tracked(pattern=None):
    args = ['git', '-C', str(ROOT), 'ls-files', '--cached', '--others', '--exclude-standard']
    if pattern:
        args.append(pattern)
    return [line for line in subprocess.check_output(args, text=True).splitlines() if line]


def markdown_files():
    return [name for name in tracked('*.md') if not name.startswith(EXCLUDED)]


def slug(heading):
    text = re.sub(r'`([^`]*)`', r'\1', heading.strip()).lower()
    text = re.sub(r'\[([^\]]*)\]\([^)]*\)', r'\1', text)
    kept = ''.join(c for c in text if c in ' -_' or unicodedata.category(c)[0] in 'LN')
    return kept.replace(' ', '-')


def anchors(path):
    seen, result = {}, set()
    fence = False
    for line in path.read_text(encoding='utf-8').splitlines():
        if line.startswith('```'):
            fence = not fence
            continue
        m = None if fence else re.match(r'#{1,6}\s+(.*?)\s*#*\s*$', line)
        if m:
            base = slug(m.group(1))
            n = seen.get(base, 0)
            seen[base] = n + 1
            result.add(base if n == 0 else f'{base}-{n}')
    return result


def links(path):
    fence = False
    for number, line in enumerate(path.read_text(encoding='utf-8').splitlines(), 1):
        if line.startswith('```'):
            fence = not fence
            continue
        if not fence:
            for m in LINK.finditer(re.sub(r'`[^`]*`', '', line)):
                yield number, m.group(1)


def check_links(files, pending, every):
    errors, used = [], set()
    known = set(every)
    for name in files:
        source = ROOT / name
        for number, target in links(source):
            if re.match(r'[a-z][a-z0-9+.-]*:', target):
                continue
            file_part, _, anchor = target.partition('#')
            dest = (source.parent / file_part).resolve() if file_part else source
            try:
                rel = dest.relative_to(ROOT).as_posix()
            except ValueError:
                errors.append(f'{name}:{number}: {target}: outside the repository')
                continue
            key = f'{name} {target}'
            if rel not in known and not dest.is_dir():
                if key in pending:
                    used.add(key)
                    continue
                errors.append(f'{name}:{number}: {target}: no such file')
                continue
            if anchor and dest.suffix == '.md' and anchor not in anchors(dest):
                if key in pending:
                    used.add(key)
                    continue
                errors.append(f'{name}:{number}: {target}: no such heading')
    for key in sorted(pending - used):
        errors.append(f'{PENDING.relative_to(ROOT)}: {key}: resolves or is gone; remove it')
    return errors


def spec_rows(text):
    """Yield (line number, header, cells) for every table body row."""
    header = None
    for number, line in enumerate(text.splitlines(), 1):
        if not line.startswith('|'):
            header = None
            continue
        cells = [c.strip() for c in re.split(r'(?<!\\)\|', line.strip())[1:-1]]
        if header is None:
            header = [c.lower() for c in cells]
            continue
        if all(re.fullmatch(r':?-+:?', c) for c in cells):
            continue
        yield number, header, cells


def check_specification(name, text):
    """Return (errors, defined IDs) for a specification document."""
    errors, defined = [], {}
    for number, line in enumerate(text.splitlines(), 1):
        if DATE.search(line):
            errors.append(f'{name}:{number}: date')
        if RELEASE.search(line):
            errors.append(f'{name}:{number}: release number')
        if PEOPLE.search(line):
            errors.append(f'{name}:{number}: reference to a person')
    for number, header, cells in spec_rows(text):
        if header == ['id', 'repository']:
            rid = cells[0] if cells else ''
            if not ROW_ID.fullmatch(rid) or len(cells) < 2 or not cells[1]:
                errors.append(f'{name}:{number}: an external ID row needs an ID and a repository')
            elif rid in defined:
                errors.append(f'{name}:{number}: {rid} is also defined on line {defined[rid]}')
            else:
                defined[rid] = number
            continue
        if header[:3] != ['id', 'requirement', 'acceptance']:
            errors.append(f'{name}:{number}: table header must start with ID | Requirement | Acceptance')
            continue
        row = dict(zip(header, cells))
        rid = row.get('id', '')
        if not ROW_ID.fullmatch(rid):
            errors.append(f'{name}:{number}: {rid!r} is not a requirement ID')
            continue
        if rid in defined:
            errors.append(f'{name}:{number}: {rid} is also defined on line {defined[rid]}')
        defined.setdefault(rid, number)
        if not row.get('requirement'):
            errors.append(f'{name}:{number}: {rid} has no requirement')
        if not row.get('acceptance'):
            errors.append(f'{name}:{number}: {rid} has no acceptance')
    return errors, defined


def check_citations(defined, files):
    """Every cited ID of a defined family must be defined."""
    families = {rid.rsplit('-', 1)[0] for rid in defined}
    errors = []
    for name in files:
        if name == SPEC or name.startswith(EXCLUDED) or not name.startswith(CITING):
            continue
        path = ROOT / name
        try:
            text = path.read_text(encoding='utf-8')
        except (UnicodeDecodeError, IsADirectoryError, FileNotFoundError):
            continue
        for number, line in enumerate(text.splitlines(), 1):
            for m in ID.finditer(line):
                rid = m.group(0)
                if m.group(1) in families and rid not in defined:
                    errors.append(f'{name}:{number}: {rid} is not in {SPEC}')
    return errors


def main():
    pending = set()
    if PENDING.exists():
        for line in PENDING.read_text(encoding='utf-8').splitlines():
            if line.strip() and not line.startswith('#'):
                pending.add(' '.join(line.split()))
    every = tracked()
    errors = check_links(markdown_files(), pending, every)
    spec_errors, defined = check_specification(SPEC, (ROOT / SPEC).read_text(encoding='utf-8'))
    errors += spec_errors
    errors += check_citations(defined, every)
    for error in errors:
        print(error, file=sys.stderr)
    return 1 if errors else 0


if __name__ == '__main__':
    sys.exit(main())
