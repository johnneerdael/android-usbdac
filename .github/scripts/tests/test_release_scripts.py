"""Offline release provenance and publication boundary tests."""

import hashlib
import io
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch
import zipfile

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
import prepare_release as prepare
import recover_release as recover
import release_artifact as artifact
import publish_release as publish
import release_notes as notes


class ReservationTests(unittest.TestCase):
    def test_current_and_superseded(self):
        with patch.object(prepare, 'reservations', return_value=[('new', 2), ('old', 1)]):
            prepare.require_latest_reservation('main', '0.1.12', 'new')
            with self.assertRaises(SystemExit):
                prepare.require_latest_reservation('main', '0.1.12', 'old')

    def test_all_attempts_must_skip_publishing(self):
        state = {'result': 'skipped'}
        def api(*args):
            path = args[-1]
            if 'workflow_dispatch' in path:
                return [{'workflow_runs': []}]
            number = int(path.split('/runs/')[1].split('/')[0])
            run = {'id': number, 'run_attempt': 2, 'status': 'completed', 'conclusion': 'failure'}
            if '/jobs?' in path:
                jobs = [] if state['result'] == 'missing' else [{'name': prepare.PACKAGE_JOB,
                    'status': 'in_progress' if state['result'] == 'active' else 'completed',
                    'conclusion': None if state['result'] == 'active' else state['result']}]
                return [{'jobs': jobs}]
            if '/attempts/' in path:
                run['run_attempt'] = int(path.split('/attempts/')[1])
                return run
            return dict(run, head_sha='c' * 40, path='.github/workflows/release.yml',
                        event='push', head_branch='main', repository={'full_name': 'x/y'},
                        head_repository={'full_name': 'x/y'}, created_at='2026-11-01T00:00:00Z')
        with patch.object(prepare, 'api_json', side_effect=api), \
             patch.object(prepare, 'git', return_value='c' * 40), \
             patch.object(prepare, 'trusted_workflow', return_value=True):
            records = [('a' * 40, 1), ('b' * 40, 2)]
            self.assertTrue(prepare.never_published('x/y', '0.1.12', records))
            for outcome in ('missing', 'active', 'failure', 'success'):
                state['result'] = outcome
                self.assertFalse(prepare.never_published('x/y', '0.1.12', records))

    def test_uncertain_evidence_does_not_authorize_adoption(self):
        with patch.object(prepare, 'reservations', return_value=[('a', 1)]), \
             patch.object(prepare, 'never_published', side_effect=OSError('offline')), \
             patch.dict(prepare.os.environ, {'GITHUB_REPOSITORY': 'x/y'}):
            self.assertFalse(prepare.can_adopt('main', '0.1.12', '0.1.11'))


class ReceiptTests(unittest.TestCase):
    def check_receipt(self, schema, historical=False, identity='usbdac-release-gates/v1',
                      changed_digest=False, source_repo='x/y', source_repo_id=17):
        candidate, workflow = 'b' * 40, 'a' * 40
        run = {'head_sha': workflow, 'repository': {'id': 17}}
        jobs = [{'name': name, 'status': 'completed', 'conclusion': 'success', 'id': i}
                for i, name in enumerate((recover.PREPARE, 'Seal release gate evidence', *recover.GATES))]
        digest = 'sha256:' + 'c' * 64
        items = [{'name': f'release-payload-42-1-{candidate}', 'id': 9, 'digest': digest,
                  'workflow_run': {'id': 42}},
                 {'name': 'release-gate-receipt-42-1', 'id': 10, 'workflow_run': {'id': 42}}]
        receipt = dict(schema_version=schema, repository=source_repo, version='0.1.11',
            source_sha=candidate, workflow_sha=workflow, run_id=42, run_attempt=1,
            artifact_id=9, artifact_digest=('sha256:' + 'e' * 64 if changed_digest else digest),
            reservation_run_id=42, manifest_sha256='d' * 64)
        if schema == 2:
            receipt['gate_identity'] = identity
        def command(args):
            if args[0] == 'git':
                return b'historical-layout' if historical else Path(args[-1].split(':', 1)[1]).read_bytes()
            data = io.BytesIO()
            with zipfile.ZipFile(data, 'w') as archive:
                archive.writestr('receipt.json', json.dumps(receipt))
            return data.getvalue()
        def api(*args):
            if args == (f'repos/{source_repo}',):
                return json.dumps({'id': source_repo_id})
            return f'[command] git log -1 --format=%H\n2026-11-01T00:00:00Z {candidate}\n'
        with patch.object(recover.subprocess, 'check_output', side_effect=command), \
             patch.object(recover, 'api', side_effect=api):
            return recover.verify_staged(run, jobs, items, 'x/y', '0.1.11', candidate, 42, 1, 42)

    def test_schema_two_identity_and_digest(self):
        self.assertEqual(self.check_receipt(2)[0]['id'], 9)
        with self.assertRaises(SystemExit):
            self.check_receipt(2, identity='unknown')
        with self.assertRaises(SystemExit):
            self.check_receipt(2, changed_digest=True)

    def test_schema_one_pinned_historical_shape(self):
        self.assertEqual(self.check_receipt(1, historical=True)[0]['id'], 9)

    def test_receipt_repository_rename_requires_the_same_repository_id(self):
        self.assertEqual(self.check_receipt(2, source_repo='x/previous')[2]['repository'], 'x/previous')
        with self.assertRaises(SystemExit):
            self.check_receipt(2, source_repo='x/previous', source_repo_id=18)

    def test_recovery_reuses_only_verified_validator_artifacts(self):
        jobs = [dict(name=f'Build the {os} validator', status='completed', conclusion='success')
                for os in ('Android', 'macOS')]
        artifacts = [dict(name=f'{platform}-validator-42-1', id=index, digest='sha256:' + 'a' * 64,
                          workflow_run={'id': 42}) for index, platform in enumerate(('android', 'mac'), 9)]
        self.assertEqual(recover.validator_artifacts(jobs, artifacts, 42, 1), {'android': 9, 'mac': 10})
        artifacts[0]['workflow_run']['id'] = 43
        with self.assertRaises(SystemExit):
            recover.validator_artifacts(jobs, artifacts, 42, 1)


class PayloadAndPublicationTests(unittest.TestCase):
    def test_staged_file_digest_mismatch(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            entries = {}
            for name in artifact.required(root, '0.1.11'):
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(b'original')
                entries[name] = {'sha256': hashlib.sha256(b'original').hexdigest(), 'size': 8}
            (root / 'manifest.json').write_text(json.dumps({'schema_version': 1,
                'version': '0.1.11', 'files': entries}))
            artifact.verify(root, {'version': '0.1.11'})
            (root / next(iter(entries))).write_bytes(b'modified')
            with self.assertRaises(SystemExit):
                artifact.verify(root, {'version': '0.1.11'})

    def test_maven_matching_partial_and_conflicting(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            folder = root / 'maven/dev/usbdac/usbdac/0.1.11'
            folder.mkdir(parents=True)
            for suffix in ('.aar', '.pom', '.module', '-sources.jar'):
                (folder / f'usbdac-0.1.11{suffix}').write_bytes(b'content')
            with patch.object(publish, 'fetch', return_value=b'content'):
                self.assertTrue(publish.classify_maven(root, '0.1.11', 'x/y', 'auth', 'usbdac'))
            with patch.object(publish, 'fetch', side_effect=[b'content', None, None, None]):
                with self.assertRaises(SystemExit):
                    publish.classify_maven(root, '0.1.11', 'x/y', 'auth', 'usbdac')
            with patch.object(publish, 'fetch', return_value=b'conflict'):
                with self.assertRaises(SystemExit):
                    publish.classify_maven(root, '0.1.11', 'x/y', 'auth', 'usbdac')

    def test_release_notes_bind_coordinates_and_provenance(self):
        text = notes.render('0.1.11', 'a' * 40, ['- Fix'], 'x/y', True)
        for fragment in ('dev.usbdac:usbdac:0.1.11', '/releases/download/v0.1.11/manifest.json',
                         '/releases/download/v0.1.11/receipt.json', '/commit/' + 'a' * 40,
                         'usbdac-validator-0.1.11-macos-arm64.zip'):
            self.assertIn(fragment, text)


if __name__ == '__main__':
    unittest.main()
