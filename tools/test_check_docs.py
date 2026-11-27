#!/usr/bin/env python3
"""Host checks for tools/check_docs.py on generated documents."""
import pathlib
import sys
import tempfile
import unittest

HERE = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import check_docs  # noqa: E402

HEADER = '| ID | Requirement | Acceptance |\n| --- | --- | --- |\n'


class SpecificationTest(unittest.TestCase):
    def check(self, body):
        return check_docs.check_specification('spec.md', body)

    def test_accepts_well_formed_rows(self):
        errors, defined = self.check(HEADER + '| UX-001 | Lists scroll. | A list scrolls. |\n')
        self.assertEqual(errors, [])
        self.assertEqual(set(defined), {'UX-001'})

    def test_rejects_a_repeated_id(self):
        errors, _ = self.check(HEADER + '| UX-001 | A. | B. |\n| UX-001 | C. | D. |\n')
        self.assertTrue(any('also defined' in e for e in errors))

    def test_rejects_a_missing_acceptance(self):
        errors, _ = self.check(HEADER + '| UX-001 | A. |  |\n')
        self.assertTrue(any('no acceptance' in e for e in errors))

    def test_rejects_dates_releases_and_people(self):
        for text in ('Added 2026-11-02.', 'Since V0.44.', 'Requested by the owner.', 'As he asked.'):
            errors, _ = self.check(HEADER + f'| UX-001 | {text} | A. |\n')
            self.assertTrue(errors, text)

    def test_rejects_another_table_shape(self):
        errors, _ = self.check('| ID | Text |\n| --- | --- |\n| UX-001 | A. |\n')
        self.assertTrue(any('table header' in e for e in errors))

    def test_external_ids_count_as_defined(self):
        errors, defined = self.check('| ID | Repository |\n| --- | --- |\n| POWER-002 | q2-os |\n')
        self.assertEqual(errors, [])
        self.assertIn('POWER-002', defined)


class CitationTest(unittest.TestCase):
    def test_flags_an_undefined_id_of_a_known_family(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            (root / 'docs').mkdir()
            (root / 'docs' / 'a.md').write_text('See UX-001, UX-002 and SHA-256.\n')
            saved = check_docs.ROOT
            check_docs.ROOT = root
            try:
                errors = check_docs.check_citations({'UX-001': 1}, ['docs/a.md'])
            finally:
                check_docs.ROOT = saved
        self.assertEqual(errors, ['docs/a.md:1: UX-002 is not in docs/specification.md'])


class DocPathTest(unittest.TestCase):
    def test_flags_a_missing_document_but_not_another_repository(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            (root / 'src').mkdir()
            (root / 'docs').mkdir()
            (root / 'docs' / 'a.md').write_text('# A\n')
            (root / 'src' / 'x.rs').write_text(
                '// docs/a.md, docs/gone.md, q2-os docs/hardware.md, q2-toolchain `docs/b.md`\n'
                '// https://github.com/q2-nova/q2-os/blob/main/docs/c.md\n'
                'Measured in q2-os\n[docs/d.md](https://example.org/d)\n')
            saved = check_docs.ROOT
            check_docs.ROOT = root
            try:
                errors = check_docs.check_doc_paths(['docs/a.md', 'src/x.rs'])
            finally:
                check_docs.ROOT = saved
        self.assertEqual(errors, ['src/x.rs:1: docs/gone.md: no such document'])


class LinkTest(unittest.TestCase):
    def test_slug_matches_github_headings(self):
        self.assertEqual(check_docs.slug('Audio and sound'), 'audio-and-sound')
        self.assertEqual(check_docs.slug('Requests, timeouts and retries'), 'requests-timeouts-and-retries')


if __name__ == '__main__':
    unittest.main()
