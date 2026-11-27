import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock


SPEC = importlib.util.spec_from_file_location('stock_draft',
    Path(__file__).resolve().parents[2] / 'tools/stock-draft.py')
DRAFT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(DRAFT)
TAG = 'stock-v0.1.0-rc.1'
COMMIT = 'a' * 40


class StockDraftTest(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.directory = Path(temp.name)
        package = self.directory / DRAFT.PAYLOADS[0]
        package.write_bytes(b'generated archive')
        (self.directory / 'player-source.tar.gz').write_bytes(b'generated source')
        self.report = {
            'player_tag': TAG, 'player_commit': COMMIT, 'version': 'V0.02',
            'profile': 'stock-qualification-candidate', 'installed_root_ready': True,
            'player_ready': None, 'hardware_qualified': False,
            'update_tar_sha256': DRAFT.digest(package),
        }
        self.write_report()

    def write_report(self):
        (self.directory / 'result.json').write_text(json.dumps(self.report))
        (self.directory / 'SHA256SUMS').write_text(''.join(
            f'{DRAFT.digest(self.directory / name)}  {name}\n' for name in DRAFT.PAYLOADS))

    def test_rejects_tampered_payload_before_api(self):
        (self.directory / DRAFT.PAYLOADS[0]).write_bytes(b'changed bytes')
        with mock.patch.object(DRAFT, 'api') as api, self.assertRaisesRegex(ValueError, 'SHA256SUMS'):
            DRAFT.stage(self.directory, 'example/player', TAG, COMMIT)
        api.assert_not_called()

    def test_rejects_claim_of_physical_qualification(self):
        self.report['hardware_qualified'] = True
        self.write_report()
        with self.assertRaisesRegex(ValueError, 'unqualified archive'):
            DRAFT.admit(self.directory, TAG, COMMIT)

    def test_refuses_to_modify_an_existing_draft(self):
        with mock.patch.object(DRAFT, 'api', return_value=[[{'tag_name': TAG}]]) as api:
            with self.assertRaisesRegex(ValueError, 'already uses'):
                DRAFT.stage(self.directory, 'example/player', TAG, COMMIT)
        self.assertEqual(api.call_count, 1)

    def test_uploads_only_by_new_draft_id(self):
        endpoint = 'repos/example/player/releases'
        assets = [{
            'name': name, 'digest': f'sha256:{DRAFT.digest(self.directory / name)}'
        } for name in (*DRAFT.PAYLOADS, 'SHA256SUMS')]
        with mock.patch.object(DRAFT, 'api', side_effect=[
            [[]], {'id': 131, 'tag_name': TAG, 'draft': True, 'prerelease': True},
            *assets, {'draft': True, 'html_url': 'https://example.invalid/release'},
        ]) as api, mock.patch('builtins.print'):
            DRAFT.stage(self.directory, 'example/player', TAG, COMMIT)
        self.assertIn('draft=true', api.call_args_list[1].args)
        self.assertIn('make_latest=false', api.call_args_list[1].args)
        self.assertEqual(
            [call.args[0] for call in api.call_args_list[2:-1]],
            [f'https://uploads.github.com/{endpoint}/131/assets?name={name}'
             for name in (*DRAFT.PAYLOADS, 'SHA256SUMS')])
        self.assertEqual(api.call_args_list[-1].args, (f'{endpoint}/131',))
