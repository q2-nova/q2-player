"""tools/third_party_notices.py on generated cargo-about JSON; cargo-about itself is not run."""
from pathlib import Path
import unittest
from unittest import mock

import third_party_notices as notices

ROOT = notices.ROOT
MIT = 'MIT License\n\nCopyright (c) Example\n\nPermission is hereby granted ...'


def crate(name, version='1.0.0', license='MIT', source='registry+https://github.com/rust-lang/crates.io-index',
          manifest=None, authors=()):
    manifest = manifest or f'/registry/{name}-{version}/Cargo.toml'
    return {'package': {'name': name, 'version': version, 'id': f'{name}@{version}', 'source': source,
                        'manifest_path': str(manifest), 'authors': list(authors)}, 'license': license}


def about(crates, licenses):
    """cargo-about JSON: `licenses` is [(id, text, source_path, [crate names])]."""
    ids = {c['package']['name']: c['package'] for c in crates}
    return {'overview': [], 'crates': crates,
            'licenses': [{'id': license_id, 'name': license_id, 'text': text, 'source_path': path,
                          'used_by': [{'crate': ids[name]} for name in users]}
                         for license_id, text, path, users in licenses]}


class Packages(unittest.TestCase):
    def test_first_party_left_out_and_standard_text_marked(self):
        data = about([crate('q2nd', source=None, manifest=ROOT / 'crates/q2nd/Cargo.toml', license='GPL-3.0-or-later'),
                      crate('bytes'), crate('plain', authors=['A Person'])],
                     [('GPL-3.0-or-later', 'GPL', None, ['q2nd']),
                      ('MIT', MIT, '/registry/bytes-1.0.0/LICENSE', ['bytes']),
                      ('MIT', 'MIT standard text', None, ['plain'])])
        result = notices.packages(data, {'q2nd'})
        self.assertEqual([entry['label'] for entry in result], ['bytes 1.0.0', 'plain 1.0.0'])
        self.assertEqual(result[0]['texts'], [('MIT', MIT, 'LICENSE', False)])
        self.assertFalse(result[0]['standard'])
        self.assertTrue(result[1]['standard'])
        self.assertEqual(result[1]['authors'], ['A Person'])

    def test_first_party_must_be_a_repository_path(self):
        data = about([crate('q2nd')], [('MIT', MIT, None, ['q2nd'])])
        with self.assertRaisesRegex(SystemExit, 'first-party'):
            notices.packages(data, {'q2nd'})

    def test_package_without_text_fails(self):
        with self.assertRaisesRegex(SystemExit, 'no license text'):
            notices.packages(about([crate('bytes')], []))
        with self.assertRaisesRegex(SystemExit, 'no license text'):
            notices.packages(about([crate('bytes')], [('MIT', '  \n', None, ['bytes'])]))

    def test_license_root_replaces_standard_text_of_a_path_package(self):
        license_file = 'tools/notices/about.toml'
        root = {'path': 'tools', 'license': 'BSD-3-Clause', 'files': [license_file]}
        inner = crate('ts_util', source=None, manifest=ROOT / 'tools/x/Cargo.toml', license='BSD-3-Clause')
        outside = crate('other', source=None, manifest=ROOT / 'crates/x/Cargo.toml', license='BSD-3-Clause')
        data = about([inner, outside], [('BSD-3-Clause', 'standard BSD', None, ['ts_util', 'other'])])
        result = {entry['label']: entry for entry in notices.packages(data, (), [root])}
        self.assertEqual(result['ts_util 1.0.0']['texts'],
                         [('BSD-3-Clause', (ROOT / license_file).read_text().strip('\n'), 'about.toml', False)])
        self.assertTrue(result['other 1.0.0']['standard'])


class Render(unittest.TestCase):
    def entries(self):
        return [{'label': 'a 1.0', 'expression': 'MIT', 'texts': [('MIT', MIT, 'LICENSE', False)],
                 'authors': [], 'standard': False},
                {'label': 'b 2.0', 'expression': 'MIT OR Apache-2.0', 'texts': [('MIT', MIT, 'LICENSE-MIT', False)],
                 'authors': [], 'standard': False},
                {'label': 'c 3.0', 'expression': 'MIT', 'texts': [('MIT', 'standard', '', True)],
                 'authors': ['C Author'], 'standard': True}]

    def test_each_text_once_with_its_users(self):
        text = notices.render('q2nd', 'cargo-about 0.9.3', [('Cargo packages', self.entries()), ('Empty', [])])
        self.assertEqual(text.count('Copyright (c) Example'), 1)
        self.assertIn('Used by: a 1.0, b 2.0', text)
        self.assertIn('c 3.0*', text)
        self.assertIn('c 3.0: C Author', text)
        self.assertNotIn('Empty', text)
        self.assertRegex(text, r'a 1.0 +MIT  \[2\]')
        self.assertRegex(text, r'c 3.0\* +MIT  \[1\]')

    def test_refuses_a_local_path(self):
        entry = dict(self.entries()[0], texts=[('MIT', f'see {ROOT}/LICENSE', 'LICENSE', False)])
        with self.assertRaisesRegex(SystemExit, 'local path'):
            notices.render('q2nd', 'cargo-about 0.9.3', [('Cargo packages', [entry])])


class Generate(unittest.TestCase):
    def test_bundled_code_follows_its_package_and_std_is_listed(self):
        config = notices.load_config()
        bundled = config['bundled'][0]
        root = crate(bundled['package'], source=None, manifest=ROOT / 'crates/q2nd/Cargo.toml',
                     license='GPL-3.0-or-later')
        player = about([root, crate('bytes')], [('GPL-3.0-or-later', 'GPL', None, [bundled['package']]),
                                                ('MIT', MIT, '/registry/bytes-1.0.0/LICENSE', ['bytes'])])
        std = about([crate('core', source=None, manifest='/rust/library/core/Cargo.toml')],
                    [('MIT', 'standard MIT', None, ['core'])])
        calls = []

        def fake_about(manifest, **options):
            calls.append((Path(manifest), options))
            return std if Path(manifest) == Path('/rust/library/std/Cargo.toml') else player

        with mock.patch.object(notices, 'about', fake_about), \
                mock.patch.object(notices, 'check_tool', return_value='cargo-about 0.9.3'):
            text, summary = notices.generate('q2nd', std_library=Path('/rust/library'), std_features=['backtrace'])
        self.assertEqual(calls[0][1]['no_default_features'], True)
        self.assertEqual(calls[1], (Path('/rust/library/std/Cargo.toml'),
                                    {'features': ['backtrace'], 'toolchain': None}))
        self.assertIn(bundled['name'], summary['bundled'][0])
        self.assertEqual((summary['packages'], summary['std_packages']), (1, 1))
        self.assertEqual(summary['standard_text_only'], ['core 1.0.0'])
        self.assertIn('Rust standard library, built from source', text)
        self.assertEqual(summary['sha256'], notices.sha(text.encode()))

    def test_bundled_code_of_an_absent_package_is_left_out(self):
        self.assertEqual(notices.bundled(notices.load_config(), {'bytes'}), [])

    def test_bundled_files_exist(self):
        config = notices.load_config()
        for entry in config['bundled']:
            for file in entry['files']:
                self.assertTrue((ROOT / file).is_file(), file)
        for spec in config['binaries'].values():
            self.assertTrue((ROOT / spec['manifest']).is_file())


class Tool(unittest.TestCase):
    def test_pinned_release_required(self):
        config = {'cargo_about': '0.9.3'}
        for output in ('cargo-about 0.9.2\n', ''):
            result = mock.Mock(stdout=output)
            with mock.patch('subprocess.run', return_value=result), \
                    self.assertRaisesRegex(SystemExit, 'cargo install --locked cargo-about --version 0.9.3'):
                notices.check_tool(config)
        with mock.patch('subprocess.run', return_value=mock.Mock(stdout='cargo-about 0.9.3\n')):
            self.assertEqual(notices.check_tool(config), 'cargo-about 0.9.3')


if __name__ == '__main__':
    unittest.main()
