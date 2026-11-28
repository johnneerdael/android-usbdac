"""Offline release provenance and publication boundary tests."""

import hashlib
import os
import re
import subprocess
import textwrap
import io
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch
import zipfile

SCRIPTS = Path(__file__).resolve().parents[1]
# Resolve repository files from here, not the working directory, so the tests
# pass from the repository root, .github/scripts and .github/scripts/tests.
REPO = SCRIPTS.parent.parent
sys.path.insert(0, str(SCRIPTS))
import prepare_release as prepare
import recover_release as recover
import release_artifact as artifact
import publish_release as publish
import release_notes as notes
import pin_release_adapter as pin


class ReservationTests(unittest.TestCase):
    def test_reviewed_adapter_matches_current_files_and_rejects_changes(self):
        # Re-pinning appends or amends the newest entry, so it must be the current implementation.
        adapter = prepare.reviewed_adapters()[-1]
        self.assertEqual(adapter['schema'], 3)
        repo = REPO
        def git_show(args, **kwargs):
            path = args[-1].split(':', 1)[1]
            return (repo / path).read_bytes()
        with patch.object(prepare.subprocess, 'check_output', side_effect=git_show):
            self.assertTrue(prepare.matches_adapter('reviewed-source', adapter['files']))
            changed = dict(adapter['files'])
            changed['.github/workflows/ci.yml'] = '0' * 64
            self.assertFalse(prepare.matches_adapter('reviewed-source', changed))
            self.assertFalse(prepare.matches_adapter('reviewed-source', {}))

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
            if path.startswith('repositories/'):
                return {'id': int(path.split('/')[1]), 'full_name': 'x/y'}
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
                        event='push', head_branch='main', repository={'full_name': 'x/y', 'id': 17},
                        head_repository={'full_name': 'x/y', 'id': 17}, created_at='2026-11-01T00:00:00Z')
        records = [('a' * 40, 1), ('b' * 40, 2)]
        # Hosted runners set GITHUB_REPOSITORY_ID, which switches never_published to the
        # numeric repository check; exercise both the name and the ID path explicitly.
        for repo_id in (None, '17'):
            state['result'] = 'skipped'
            env = {k: v for k, v in prepare.os.environ.items() if k != 'GITHUB_REPOSITORY_ID'}
            if repo_id is not None:
                env['GITHUB_REPOSITORY_ID'] = repo_id
            with self.subTest(repository_id=repo_id), \
                 patch.dict(prepare.os.environ, env, clear=True), \
                 patch.object(prepare, 'api_json', side_effect=api), \
                 patch.object(prepare, 'git', return_value='c' * 40), \
                 patch.object(prepare, 'trusted_workflow', return_value=True):
                self.assertTrue(prepare.never_published('x/y', '0.1.12', records))
                for outcome in ('missing', 'active', 'failure', 'success'):
                    state['result'] = outcome
                    self.assertFalse(prepare.never_published('x/y', '0.1.12', records))

    def test_uncertain_evidence_does_not_authorize_adoption(self):
        with patch.object(prepare, 'reservations', return_value=[('a', 1)]), \
             patch.object(prepare, 'never_published', side_effect=OSError('offline')), \
             patch.dict(prepare.os.environ, {'GITHUB_REPOSITORY': 'x/y'}):
            self.assertFalse(prepare.can_adopt('main', '0.1.12', '0.1.11'))

    def test_reserved_version_requires_adoption_after_ordinary_edit(self):
        def exercise(records, adopted, absent, declared='0.1.15'):
            calls = []
            def git(*args):
                calls.append(args)
                if args[:2] == ('rev-parse', 'FETCH_HEAD'): return 'source'
                if args[:2] == ('log', 'source'): return ''
                if args[:2] == ('diff', '--name-only'): return 'changed'
                return ''
            with patch.object(prepare, 'git', side_effect=git), patch.object(prepare, 'stable_tags', return_value=['v0.1.14']), \
                 patch.object(prepare, 'declared_version', return_value=declared), patch.object(prepare, 'reservations', return_value=records), \
                 patch.object(prepare, 'can_adopt', return_value=adopted), \
                 patch.object(prepare, 'registry_absent', **({'side_effect': absent} if isinstance(absent, Exception) else {'return_value': absent})), \
                 patch.object(prepare, 'set_version'), patch.object(prepare.subprocess, 'run') as push, \
                 patch.dict(prepare.os.environ, {'GITHUB_REPOSITORY': 'x/y'}):
                push.return_value.returncode = 0
                return prepare.select_automatic(42, 'source')
        self.assertEqual(exercise([('reserved', 1)], False, True), '0.1.16')
        self.assertEqual(exercise([('reserved', 1)], True, False), '0.1.15')
        self.assertEqual(exercise([], False, True), '0.1.15')
        self.assertEqual(exercise([], False, True, declared='0.2.0'), '0.2.0')
        # An unreserved declared version newer than every tag is the maintainer's
        # choice. If the registries cannot prove it unpublished, stop rather than
        # replace 0.2.0 with its first patch.
        for declared in ('0.1.15', '0.2.0'):
            for absent in (False, OSError('registry unreachable')):
                with self.subTest(declared=declared, absent=absent), self.assertRaises(SystemExit):
                    exercise([], False, absent, declared=declared)
        # The tagged version itself is released, so main moves to the next patch.
        self.assertEqual(exercise([], False, False, declared='0.1.14'), '0.1.15')


# A crate, Maven or AAR coordinate that names a release version.
COORDINATE = re.compile(r'usbdac-core(?:@|\s*=\s*"|\s*=\s*\{[^}\n]*version\s*=\s*"|-(?=[0-9.]+\.crate))([0-9]+\.[0-9]+\.[0-9]+)'
                        r'|dev\.usbdac:usbdac(?:-media3)?:([0-9]+\.[0-9]+\.[0-9]+)'
                        r'|usbdac(?:-media3)?-([0-9]+\.[0-9]+\.[0-9]+)\.aar')


class VersionedDocsTests(unittest.TestCase):
    def test_every_document_naming_a_coordinate_is_rewritten_at_release(self):
        tracked = subprocess.check_output(['git', 'ls-files', '-z', '--', '*.md', '*.html'], cwd=REPO).decode().split('\0')
        naming = {name for name in tracked if name and COORDINATE.search((REPO / name).read_text(errors='replace'))}
        self.assertTrue(naming)
        self.assertEqual(naming - set(prepare.VERSIONED_DOCS), set(),
                         'add these documents and their coordinates to VERSIONED_DOCS in prepare_release.py')

    def test_set_version_rewrites_every_coordinate_and_literal(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for name, templates in prepare.VERSIONED_DOCS.items():
                content = (REPO / name).read_text()
                for template in templates:
                    pattern = re.escape(template).replace(re.escape('{version}'), prepare.STABLE_VERSION.pattern)
                    self.assertRegex(content, pattern, f'{name} no longer contains {template!r}')
                (root / name).parent.mkdir(parents=True, exist_ok=True)
                (root / name).write_text(content)
            previous = os.getcwd()
            os.chdir(root)
            try:
                with patch.object(prepare, 'version_files', return_value={}):
                    prepare.set_version('0.1.17', '9.8.7', '0.1.17')
            finally:
                os.chdir(previous)
            for name, templates in prepare.VERSIONED_DOCS.items():
                content = (root / name).read_text()
                self.assertEqual({v for match in COORDINATE.findall(content) for v in match if v} - {'9.8.7'}, set(), name)
                for template in templates:
                    self.assertIn(template.format(version='9.8.7'), content)


class PinTests(unittest.TestCase):
    def layout(self, root, content):
        for path in prepare.RELEASE_IMPLEMENTATION:
            (root / path).parent.mkdir(parents=True, exist_ok=True)
            (root / path).write_bytes(content + path.encode())
        return {path: hashlib.sha256(content + path.encode()).hexdigest() for path in prepare.RELEASE_IMPLEMENTATION}

    def run_pin(self, root, *args, tags=('v0.1.17',), released=()):
        with patch.object(pin, 'REPO', root), patch.object(pin, 'ADAPTERS', root / 'adapters.json'), \
             patch.object(pin, 'stable_tags', return_value=list(tags)), \
             patch.object(pin, 'matches_adapter', side_effect=lambda tag, files: files in released):
            pin.main(list(args))
        return json.loads((root / 'adapters.json').read_text())

    def test_check_fails_until_a_changed_implementation_is_pinned(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            first = self.layout(root, b'reviewed ')
            (root / 'adapters.json').write_text(json.dumps([{'files': first, 'gate_identity': 'usbdac-release-gates/v1', 'schema': 3}]))
            self.run_pin(root, '--check')
            second = self.layout(root, b'changed ')
            with self.assertRaises(SystemExit):
                self.run_pin(root, '--check')
            adapters = self.run_pin(root)
            self.assertEqual([a['files'] for a in adapters], [first, second])
            self.run_pin(root, '--check')

    def test_amend_replaces_only_an_unreleased_pin(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            released = self.layout(root, b'released ')
            unreleased = self.layout(root, b'unreleased ')
            entry = lambda files: {'files': files, 'gate_identity': 'usbdac-release-gates/v1', 'schema': 3}
            (root / 'adapters.json').write_text(json.dumps([entry(released), entry(unreleased)]))
            current = self.layout(root, b'integrated ')
            adapters = self.run_pin(root, '--amend', released=(released,))
            self.assertEqual([a['files'] for a in adapters], [released, current])
            (root / 'adapters.json').write_text(json.dumps([entry(released)]))
            for options in (dict(released=(released,)), dict(tags=())):
                with self.assertRaises(SystemExit):
                    self.run_pin(root, '--amend', **options)
                self.assertEqual(json.loads((root / 'adapters.json').read_text()), [entry(released)])


class ReceiptTests(unittest.TestCase):
    def check_receipt(self, schema, historical=False, identity='usbdac-release-gates/v1',
                      changed_digest=False, source_repo='x/y', source_repo_id=17,
                      payload_attempt=1, receipt_attempt=1, sealed_attempt=None):
        candidate, workflow = 'b' * 40, 'a' * 40
        run = {'head_sha': workflow, 'repository': {'id': 17}}
        jobs = [{'name': name, 'status': 'completed', 'conclusion': 'success', 'id': i}
                for i, name in enumerate((recover.PREPARE, 'Seal release gate evidence', *recover.GATES))]
        digest = 'sha256:' + 'c' * 64
        items = [{'name': f'release-payload-42-{payload_attempt}-{candidate}', 'id': 9, 'digest': digest,
                  'workflow_run': {'id': 42}},
                 {'name': f'release-gate-receipt-42-{receipt_attempt}', 'id': 10, 'workflow_run': {'id': 42}}]
        receipt = dict(schema_version=schema, repository=source_repo, version='0.1.11',
            source_sha=candidate, workflow_sha=workflow, run_id=42, run_attempt=payload_attempt,
            artifact_id=9, artifact_digest=('sha256:' + 'e' * 64 if changed_digest else digest),
            reservation_run_id=42, manifest_sha256='d' * 64)
        if schema in (2, 3):
            receipt['gate_identity'] = identity
        if schema == 3:
            receipt['repository_id'] = 17
        if sealed_attempt is not None:
            receipt['receipt_run_attempt'] = sealed_attempt
        def command(args, **kwargs):
            if args[0] == 'git':
                return b'historical-layout' if historical else (REPO / args[-1].split(':', 1)[1]).read_bytes()
            data = io.BytesIO()
            with zipfile.ZipFile(data, 'w') as archive:
                archive.writestr('receipt.json', json.dumps(receipt))
            items[1]['digest'] = 'sha256:' + hashlib.sha256(data.getvalue()).hexdigest()
            return data.getvalue()
        def api(*args):
            if args == (f'repos/{source_repo}',):
                return json.dumps({'id': source_repo_id})
            return f'[command] git log -1 --format=%H\n2026-11-01T00:00:00Z {candidate}\n'
        with patch.object(recover.subprocess, 'check_output', side_effect=command), \
             patch.object(recover, 'api', side_effect=api):
            with patch.object(recover, 'reviewed_adapters', return_value=[{'schema': 3, 'files': {}}]), \
                 patch.object(recover, 'matches_adapter', return_value=not historical):
                return recover.verify_staged(run, jobs, items, 'x/y', '0.1.11', candidate, 42, receipt_attempt, 42)

    def test_main_reuses_receipt_from_original_attempt(self):
        candidate, parent = 'b' * 40, 'a' * 40
        run = dict(id=42, run_attempt=2, path='.github/workflows/release.yml', event='push', head_branch='main',
                   status='completed', conclusion='failure', head_sha=parent,
                   repository={'id': 17}, head_repository={'id': 17})
        prepare_job = dict(name=recover.PREPARE, status='completed', conclusion='success', id=100)
        def api(*args):
            path = args[-1]
            if path == 'repos/x/y': return json.dumps({'id': 17})
            if path == 'repos/x/y/actions/runs/42': return json.dumps(run)
            if '/attempts/1/jobs' in path: return json.dumps([{'jobs': [prepare_job]}])
            if '/artifacts?' in path: return json.dumps([{'artifacts': [{'name': 'release-gate-receipt-42-1'}]}])
            if '/actions/jobs/100/logs' in path: return f'Release 0.1.11 from {candidate}'
            raise AssertionError(path)
        def git(*args):
            if args[:2] == ('log', 'origin/main'): return candidate
            if args[:3] == ('show', '-s', '--format=%B'): return 'Release-Run: 42'
            if args[:3] == ('show', '-s', '--format=%P'): return parent
            if args[0] == 'show': return 'version = "0.1.11"'
            return ''
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / 'out'
            args = ['recover_release.py', '--failed-run-id', '42', '--version', '0.1.11', '--output', str(output)]
            with patch.object(sys, 'argv', args), patch.dict(recover.os.environ, {'GITHUB_EVENT_NAME': 'workflow_dispatch', 'GITHUB_REF': 'refs/heads/main', 'GITHUB_REPOSITORY': 'x/y'}), \
                 patch.object(recover, 'api', side_effect=api), patch.object(recover, 'git', side_effect=git), \
                 patch.object(recover, 'require_latest_reservation'), \
                 patch.object(recover, 'version_files', return_value={Path('Cargo.toml'): (prepare.WORKER_VERSION, 1)}), \
                 patch.object(recover.subprocess, 'run') as cat_file, \
                 patch.object(recover, 'verify_staged', return_value=({'id': 9}, {'id': 10}, {'repository': 'x/y', 'schema_version': 2, 'run_attempt': 1})) as staged:
                cat_file.return_value.returncode = 1
                recover.main()
                self.assertEqual(staged.call_args.args[7], 1)
                self.assertIn('source_run_attempt=1', output.read_text())
                self.assertIn('receipt_run_attempt=1', output.read_text())

    def test_main_recovers_earlier_receipt_with_mixed_validator_attempts(self):
        candidate, parent = 'b' * 40, 'a' * 40
        run = dict(id=42, run_attempt=3, path='.github/workflows/release.yml', event='push', head_branch='main',
                   status='completed', conclusion='failure', head_sha=parent,
                   repository={'id': 17}, head_repository={'id': 17})
        prepare_job = dict(name=recover.PREPARE, status='completed', conclusion='success', id=100)
        validator_jobs = {1: dict(name='Build the Android validator', status='completed', conclusion='success'),
                          2: dict(name='Build the macOS validator', status='completed', conclusion='success')}
        items, subjects = [], []
        for platform, attempt, artifact_id in (('android', 1, 9), ('mac', 2, 10)):
            digest = 'sha256:' + ('a' if platform == 'android' else 'b') * 64
            items.append(dict(name=f'{platform}-validator-42-{attempt}', id=artifact_id, digest=digest, workflow_run={'id': 42}))
            subjects.append(dict(platform=platform, artifact_id=artifact_id, artifact_digest=digest, run_id=42,
                                 run_attempt=attempt, source_sha=candidate, version='0.1.11', repository_id=17))
        items.append({'name': 'release-gate-receipt-42-2'})
        receipt = dict(repository='x/y', schema_version=3, validators=subjects, run_attempt=2)
        def api(*args):
            path = args[-1]
            if path == 'repos/x/y': return json.dumps({'id': 17})
            if path == 'repos/x/y/actions/runs/42': return json.dumps(run)
            if '/attempts/1/jobs' in path: return json.dumps([{'jobs': [prepare_job, validator_jobs[1]]}])
            if '/attempts/2/jobs' in path: return json.dumps([{'jobs': [validator_jobs[2]]}])
            if '/artifacts?' in path: return json.dumps([{'artifacts': items}])
            if '/actions/jobs/100/logs' in path: return f'Release 0.1.11 from {candidate}'
            raise AssertionError(path)
        def git(*args):
            if args[:2] == ('log', 'origin/main'): return candidate
            if args[:3] == ('show', '-s', '--format=%B'): return 'Release-Run: 42'
            if args[:3] == ('show', '-s', '--format=%P'): return parent
            if args[0] == 'show': return 'version = "0.1.11"'
            return ''
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / 'out'
            args = ['recover_release.py', '--failed-run-id', '42', '--version', '0.1.11', '--output', str(output)]
            with patch.object(sys, 'argv', args), patch.dict(recover.os.environ, {'GITHUB_EVENT_NAME': 'workflow_dispatch', 'GITHUB_REF': 'refs/heads/main', 'GITHUB_REPOSITORY': 'x/y'}), \
                 patch.object(recover, 'api', side_effect=api), patch.object(recover, 'git', side_effect=git), \
                 patch.object(recover, 'require_latest_reservation'), \
                 patch.object(recover, 'version_files', return_value={Path('Cargo.toml'): (prepare.WORKER_VERSION, 1)}), \
                 patch.object(recover.subprocess, 'run') as cat_file, \
                 patch.object(recover, 'verify_staged', return_value=({'id': 11}, {'id': 12}, receipt)) as staged:
                cat_file.return_value.returncode = 0
                recover.main()
                self.assertEqual(staged.call_args.args[7], 2)
                result = output.read_text()
                self.assertIn('source_run_attempt=2', result)
                self.assertIn('android_validator_id=9', result)
                self.assertIn('mac_validator_id=10', result)
                subjects[0]['run_attempt'] = 4
                with self.assertRaises(SystemExit):
                    recover.main()

    def test_recovery_accepts_a_receipt_sealed_after_its_payload(self):
        # gate-receipt failed in attempt 1 and was re-run alone in attempt 2.
        payload, _, evidence = self.check_receipt(3, payload_attempt=1, receipt_attempt=2, sealed_attempt=2)
        self.assertEqual((payload['id'], evidence['run_attempt']), (9, 1))
        # Receipts sealed before receipt_run_attempt existed (0.1.17) use their payload's attempt.
        self.assertEqual(self.check_receipt(3, payload_attempt=2, receipt_attempt=2)[0]['id'], 9)
        for attempts in (dict(payload_attempt=1, receipt_attempt=2), dict(payload_attempt=3, receipt_attempt=2, sealed_attempt=2),
                         dict(payload_attempt=1, receipt_attempt=2, sealed_attempt=1)):
            with self.subTest(**attempts), self.assertRaises(SystemExit):
                self.check_receipt(3, **attempts)

    def test_main_reports_payload_and_receipt_attempts_separately(self):
        candidate, parent = 'b' * 40, 'a' * 40
        run = dict(id=42, run_attempt=2, path='.github/workflows/release.yml', event='push', head_branch='main',
                   status='completed', conclusion='failure', head_sha=parent,
                   repository={'id': 17}, head_repository={'id': 17})
        prepare_job = dict(name=recover.PREPARE, status='completed', conclusion='success', id=100)
        def api(*args):
            path = args[-1]
            if path == 'repos/x/y': return json.dumps({'id': 17})
            if path == 'repos/x/y/actions/runs/42': return json.dumps(run)
            if '/jobs?' in path: return json.dumps([{'jobs': [prepare_job]}])
            if '/artifacts?' in path: return json.dumps([{'artifacts': [{'name': 'release-gate-receipt-42-2'}]}])
            if '/actions/jobs/100/logs' in path: return f'Release 0.1.11 from {candidate}'
            raise AssertionError(path)
        def git(*args):
            if args[:2] == ('log', 'origin/main'): return candidate
            if args[:3] == ('show', '-s', '--format=%B'): return 'Release-Run: 42'
            if args[:3] == ('show', '-s', '--format=%P'): return parent
            if args[0] == 'show': return 'version = "0.1.11"'
            return ''
        evidence = {'repository': 'x/y', 'schema_version': 3, 'validators': [], 'run_attempt': 1, 'receipt_run_attempt': 2}
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / 'out'
            args = ['recover_release.py', '--failed-run-id', '42', '--version', '0.1.11', '--output', str(output)]
            with patch.object(sys, 'argv', args), patch.dict(recover.os.environ, {'GITHUB_EVENT_NAME': 'workflow_dispatch', 'GITHUB_REF': 'refs/heads/main', 'GITHUB_REPOSITORY': 'x/y'}), \
                 patch.object(recover, 'api', side_effect=api), patch.object(recover, 'git', side_effect=git), \
                 patch.object(recover, 'require_latest_reservation'), \
                 patch.object(recover, 'version_files', return_value={Path('Cargo.toml'): (prepare.WORKER_VERSION, 1)}), \
                 patch.object(recover.subprocess, 'run') as cat_file, \
                 patch.object(recover, 'verify_staged', return_value=({'id': 9}, {'id': 10}, evidence)) as staged:
                cat_file.return_value.returncode = 1
                recover.main()
                self.assertEqual(staged.call_args.args[7], 2)
                result = output.read_text()
                self.assertIn('source_run_attempt=1', result)
                self.assertIn('receipt_run_attempt=2', result)

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

    def sealed_payload(self, root, attempt):
        entries = {}
        for name in artifact.required(root, '0.1.11'):
            path = root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b'payload')
            entries[name] = {'sha256': hashlib.sha256(b'payload').hexdigest(), 'size': 7}
        (root / 'manifest.json').write_text(json.dumps(dict(schema_version=1, repository='x/y', repository_id=17,
            version='0.1.11', source_sha='b' * 40, workflow_sha='a' * 40, run_id=42, run_attempt=attempt,
            reservation_run_id=42, files=entries)))

    def seal_env(self, **changes):
        return dict(dict(GITHUB_REPOSITORY='x/y', GITHUB_REPOSITORY_ID='17', SOURCE_SHA='b' * 40, WORKFLOW_SHA='a' * 40,
                         SOURCE_RUN_ID='42', PAYLOAD_RUN_ATTEMPT='1', RECEIPT_RUN_ATTEMPT='2', RESERVATION_RUN_ID='42',
                         GATE_IDENTITY='usbdac-release-gates/v1', ARTIFACT_ID='9', ARTIFACT_DIGEST='c' * 64,
                         VALIDATOR='false'), **changes)

    def test_seal_binds_the_payload_attempt_after_the_receipt_is_rerun(self):
        # Attempt 2 re-ran only gate-receipt; the gates kept attempt 1's payload and outputs.
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / 'payload'
            self.sealed_payload(root, 1)
            info = {'name': f"release-payload-42-1-{'b' * 40}", 'workflow_run': {'id': 42}, 'digest': 'sha256:' + 'c' * 64}
            api = lambda path: json.dumps(info).encode()
            receipt = artifact.seal(root, Path(tmp) / 'receipt' / 'receipt.json', '0.1.11', self.seal_env(), api)
            self.assertEqual((receipt['run_attempt'], receipt['receipt_run_attempt']), (1, 2))
            self.assertEqual(json.loads((Path(tmp) / 'receipt' / 'receipt.json').read_text()), receipt)
            self.assertEqual(receipt['manifest_sha256'], hashlib.sha256((root / 'manifest.json').read_bytes()).hexdigest())

    def test_seal_rejects_a_payload_attempt_its_artifact_or_manifest_does_not_carry(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / 'payload'
            self.sealed_payload(root, 1)
            for name_attempt, env in ((1, self.seal_env(PAYLOAD_RUN_ATTEMPT='2')),   # artifact name says 1
                                      (2, self.seal_env(PAYLOAD_RUN_ATTEMPT='2')),   # manifest says 1
                                      (3, self.seal_env(PAYLOAD_RUN_ATTEMPT='3')),   # later than the receipt
                                      (1, self.seal_env(PAYLOAD_RUN_ATTEMPT=''))):
                info = {'name': f"release-payload-42-{name_attempt}-{'b' * 40}", 'workflow_run': {'id': 42}, 'digest': 'sha256:' + 'c' * 64}
                with self.subTest(name_attempt=name_attempt, env=env['PAYLOAD_RUN_ATTEMPT']), self.assertRaises(SystemExit):
                    artifact.seal(root, Path(tmp) / 'receipt.json', '0.1.11', env, lambda path: json.dumps(info).encode())
            self.assertFalse((Path(tmp) / 'receipt.json').exists())

    def test_package_checks_receipt_attempt_separately_and_accepts_older_receipts(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.sealed_payload(root, 1)
            manifest = root / 'manifest.json'
            expected = dict(repository='x/y', version='0.1.11', source_sha='b' * 40, workflow_sha='a' * 40,
                            run_id=42, run_attempt=1, reservation_run_id=42, artifact_id=9)
            receipt = dict(expected, schema_version=3, gate_identity='usbdac-release-gates/v1', repository_id=17,
                           receipt_run_attempt=2, manifest_sha256=hashlib.sha256(manifest.read_bytes()).hexdigest())
            artifact.check_receipt(receipt, manifest, expected, 2, 17)
            with self.assertRaises(SystemExit):
                artifact.check_receipt(receipt, manifest, expected, 1, 17)
            # A 0.1.17 receipt has no receipt_run_attempt: it was sealed in its payload's attempt.
            older = {k: v for k, v in receipt.items() if k != 'receipt_run_attempt'}
            artifact.check_receipt(older, manifest, expected, 1, 17)
            with self.assertRaises(SystemExit):
                artifact.check_receipt(older, manifest, expected, 2, 17)
            with self.assertRaises(SystemExit):
                artifact.check_receipt(dict(receipt, repository_id=18), manifest, expected, 2, 17)

    def test_workflows_take_the_payload_attempt_from_the_gates(self):
        release = (SCRIPTS.parent / 'workflows' / 'release.yml').read_text()
        ci = (SCRIPTS.parent / 'workflows' / 'ci.yml').read_text()
        receipt_job = release.split('\n  gate-receipt:\n', 1)[1].split('\n  package:\n', 1)[0]
        self.assertIn('PAYLOAD_RUN_ATTEMPT: ${{ needs.release-gates.outputs.artifact_attempt }}', receipt_job)
        self.assertIn('RECEIPT_RUN_ATTEMPT: ${{ github.run_attempt }}', receipt_job)
        self.assertNotIn('SOURCE_RUN_ATTEMPT', receipt_job)
        self.assertIn('artifact_attempt:\n        value: ${{ jobs.android.outputs.artifact_attempt }}', ci)
        android_job = ci.split('\n  android:\n', 1)[1].split('\n    steps:\n', 1)[0]
        self.assertIn('artifact_attempt: ${{ github.run_attempt }}', android_job)

    def test_maven_matching_partial_and_conflicting(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            folder = root / 'maven/dev/usbdac/usbdac/0.1.11'
            folder.mkdir(parents=True)
            for suffix in ('.aar', '.pom', '.module', '-sources.jar'):
                (folder / f'usbdac-0.1.11{suffix}').write_bytes(b'content')
            with patch.object(publish, 'fetch', return_value=b'content'):
                self.assertEqual(publish.classify_maven(root, '0.1.11', 'x/y', 'auth', 'usbdac')['state'], 'complete_match')
            with patch.object(publish, 'fetch', side_effect=[b'content', None, None, None]):
                state = publish.classify_maven(root, '0.1.11', 'x/y', 'auth', 'usbdac')
                self.assertEqual(state['state'], 'partial_match')
                self.assertEqual(len(state['missing']), 3)
            with patch.object(publish, 'fetch', return_value=b'conflict'):
                self.assertEqual(publish.classify_maven(root, '0.1.11', 'x/y', 'auth', 'usbdac')['state'], 'conflict')

    def test_validator_receipt_binds_exact_bytes_and_archive(self):
        data = b'validator bytes'
        raw = io.BytesIO()
        with zipfile.ZipFile(raw, 'w') as z:
            z.writestr('usbdac-validator-0.1.11-android.apk', data)
        digest = 'sha256:' + hashlib.sha256(raw.getvalue()).hexdigest()
        subject = artifact.validator_subject(raw.getvalue(), 'android', 9, digest, 42, 1, '0.1.11', 'b' * 40, 17)
        self.assertEqual(subject['sha256'], hashlib.sha256(data).hexdigest())
        with self.assertRaises(SystemExit):
            artifact.validator_subject(raw.getvalue(), 'android', 9, 'sha256:' + '0' * 64, 42, 1, '0.1.11', 'b' * 40, 17)
        with zipfile.ZipFile(raw, 'a') as z:
            z.writestr('../escape', b'bad')
        with self.assertRaises(SystemExit):
            artifact.validator_subject(raw.getvalue(), 'android', 9, 'sha256:' + hashlib.sha256(raw.getvalue()).hexdigest(), 42, 1, '0.1.11', 'b' * 40, 17)

    def test_validator_subjects_allow_different_producing_attempts(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            subjects = []
            ids = {'android': 9, 'mac': 10}
            for platform, attempt, suffix in (('android', 1, 'android.apk'), ('mac', 2, 'macos-arm64.dmg')):
                filename = f'usbdac-validator-0.1.11-{suffix}'
                (root / filename).write_bytes(platform.encode())
                subjects.append(dict(platform=platform, artifact_id=ids[platform], artifact_digest='sha256:' + 'a' * 64,
                    run_id=42, run_attempt=attempt, filename=filename, size=len(platform),
                    sha256=hashlib.sha256(platform.encode()).hexdigest(), version='0.1.11', source_sha='b' * 40, repository_id=17))
            artifact.verify_validator_subjects({'validators': subjects}, root, '0.1.11', 'b' * 40, 17, 42, 2, ids)
            subjects[0]['run_attempt'] = 3
            with self.assertRaises(SystemExit):
                artifact.verify_validator_subjects({'validators': subjects}, root, '0.1.11', 'b' * 40, 17, 42, 2, ids)

    def test_crate_index_lag_after_upload_is_pending(self):
        with tempfile.TemporaryDirectory() as tmp, patch.object(publish, 'fetch', side_effect=[b'', b'archive']):
            with self.assertRaises(SystemExit):
                publish.classify_crate(Path(tmp), '0.1.11')
        with tempfile.TemporaryDirectory() as tmp, patch.object(publish, 'fetch', return_value=b''):
            self.assertFalse(publish.classify_crate(Path(tmp), '0.1.11', post_upload=True))

    def test_release_preflight_rejects_wrong_tag_and_duplicate_assets(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / 'file').write_bytes(b'x')
            notes = root / 'notes.txt'
            notes.write_text('notes')
            tag = {'ref': 'refs/tags/v0.1.11', 'object': {'type': 'commit', 'sha': 'b' * 40}}
            def wrong(url, auth):
                return json.dumps(tag).encode() if '/git/ref/' in url else None
            with patch.object(publish, 'fetch', side_effect=wrong), self.assertRaises(SystemExit):
                publish.preflight_release('x/y', '0.1.11', 'a' * 40, root, notes, 'token')
            tag['object']['sha'] = 'a' * 40
            release = {'assets': [{'name': 'file'}, {'name': 'file'}], 'body': 'notes'}
            def duplicate(url, auth):
                return json.dumps(tag if '/git/ref/' in url else release).encode()
            with patch.object(publish, 'fetch', side_effect=duplicate), self.assertRaises(SystemExit):
                publish.preflight_release('x/y', '0.1.11', 'a' * 40, root, notes, 'token')

    def test_publication_entrypoint_preflight_failure_never_uploads(self):
        env = {'VERSION': '0.1.11', 'PUBLISH_REPOSITORY': 'x/y', 'SOURCE_REPOSITORY': 'x/y',
               'SOURCE_SHA': 'a' * 40, 'WORKFLOW_SHA': 'b' * 40, 'SOURCE_RUN_ID': '42',
               'SOURCE_RUN_ATTEMPT': '1', 'RESERVATION_RUN_ID': '42', 'GITHUB_ACTOR': 'actor',
               'GITHUB_TOKEN': 'token', 'CARGO_REGISTRY_TOKEN': 'token'}
        events = []
        def classify(root, version, repo, auth, name):
            events.append(name)
            return {'state': 'absent'}
        def preflight(*args):
            events.append('release')
            raise SystemExit('conflicting tag')
        with patch.dict(publish.os.environ, env), patch.object(publish, 'verify'), \
             patch.object(publish, 'classify_maven', side_effect=classify), \
             patch.object(publish, 'classify_crate', side_effect=lambda *args: events.append('crate') or False), \
             patch.object(publish, 'preflight_release', side_effect=preflight), \
             patch.object(publish, 'publish_crate') as crate_upload, patch.object(publish, 'publish_maven') as maven_upload:
            with self.assertRaises(SystemExit):
                publish.main()
            self.assertEqual(events, ['usbdac', 'usbdac-media3', 'crate', 'release'])
            crate_upload.assert_not_called()
            maven_upload.assert_not_called()

    def test_docs_push_selects_only_published_build_inputs_and_coordinates(self):
        workflow = (SCRIPTS.parent / 'workflows' / 'docs.yml').read_text()
        program = textwrap.dedent(workflow.split("          python3 - <<'PYSELECT'\n", 1)[1].split('          PYSELECT\n', 1)[0])
        source, main = 'a' * 40, 'b' * 40
        state = {'diff': 0, 'guide': 'dev.usbdac:usbdac:0.1.16 dev.usbdac:usbdac-media3:0.1.16'}
        def command(args, **kwargs):
            if args[:2] == ['gh', 'api']: return json.dumps({'tag_name': 'v0.1.16'}).encode()
            if args[:2] == ['git', 'rev-parse']: return source + '\n'
            if args[:2] == ['git', 'show']: return state['guide']
            raise AssertionError(args)
        def compare(args, **kwargs):
            self.assertIn(source, args)
            self.assertIn(main, args)
            return type('Result', (), {'returncode': state['diff']})()
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / 'output'
            env = {'GITHUB_REF': 'refs/heads/main', 'GITHUB_EVENT_NAME': 'push', 'GITHUB_REPOSITORY': 'x/y',
                   'GITHUB_SHA': main, 'GITHUB_OUTPUT': str(output), 'VERSION': '', 'SOURCE_SHA': ''}
            with patch.dict(os.environ, env), patch('subprocess.check_output', side_effect=command), patch('subprocess.run', side_effect=compare):
                exec(compile(program, 'docs.yml:PYSELECT', 'exec'), {})
                self.assertIn('deploy=true', output.read_text())
                self.assertIn(f'source={source}', output.read_text())
                for diff, guide in ((1, state['guide']), (0, 'unpublished coordinates')):
                    state.update(diff=diff, guide=guide)
                    output.write_text('')
                    with self.assertRaises(SystemExit) as skipped:
                        exec(compile(program, 'docs.yml:PYSELECT', 'exec'), {})
                    self.assertEqual(skipped.exception.code, 0)
                    self.assertEqual(output.read_text().strip(), 'deploy=false')

    def test_docs_embedded_gate_requires_published_matching_source(self):
        workflow = (SCRIPTS.parent / 'workflows' / 'docs.yml').read_text()
        program = textwrap.dedent(workflow.split("          python3 - <<'PYDOCS'\n", 1)[1].split('          PYDOCS\n', 1)[0])
        source = 'a' * 40
        version = '0.1.16'
        manifest = {'version': version, 'source_sha': source, 'repository_id': 17, 'files': {}}
        for name in ('usbdac', 'usbdac-media3'):
            manifest['files'][f'maven/dev/usbdac/{name}/{version}/{name}-{version}.aar'] = {'sha256': 'c' * 64}
        manifest_bytes = json.dumps(manifest).encode()
        receipt = {'version': version, 'source_sha': source, 'repository_id': 17, 'manifest_sha256': hashlib.sha256(manifest_bytes).hexdigest()}
        receipt_bytes = json.dumps(receipt).encode()
        release = {'published_at': '2026-01-01T00:00:00Z', 'assets': [
            {'name': 'manifest.json', 'digest': 'sha256:' + hashlib.sha256(manifest_bytes).hexdigest()},
            {'name': 'receipt.json', 'digest': 'sha256:' + hashlib.sha256(receipt_bytes).hexdigest()},
            *[{'name': f'{name}-{version}.aar', 'digest': 'sha256:' + 'c' * 64} for name in ('usbdac', 'usbdac-media3')]]}
        state = {'tag': source}
        def command(args, **kwargs):
            if args[:2] == ['gh', 'api']: return json.dumps(release).encode()
            if args[:2] == ['git', 'rev-parse']: return state['tag'] + '\n'
            if args[:2] == ['git', 'show']: return ' '.join(f'dev.usbdac:{name}:{version}' for name in ('usbdac', 'usbdac-media3'))
            raise AssertionError(args)
        def download(args, **kwargs):
            target = Path(args[args.index('--dir') + 1])
            (target / 'manifest.json').write_bytes(manifest_bytes)
            (target / 'receipt.json').write_bytes(receipt_bytes)
        env = {'VERSION': version, 'SOURCE_SHA': source, 'GITHUB_REPOSITORY': 'x/y', 'GITHUB_REPOSITORY_ID': '17', 'GITHUB_REF': 'refs/heads/main', 'GITHUB_EVENT_NAME': 'workflow_dispatch'}
        with patch.dict(os.environ, env), patch('subprocess.check_output', side_effect=command), patch('subprocess.run', side_effect=download):
            exec(compile(program, 'docs.yml:PYDOCS', 'exec'), {})
            state['tag'] = 'b' * 40
            with self.assertRaises(SystemExit):
                exec(compile(program, 'docs.yml:PYDOCS', 'exec'), {})
            state['tag'] = source
            os.environ['GITHUB_REF'] = 'refs/heads/other'
            with self.assertRaises(SystemExit):
                exec(compile(program, 'docs.yml:PYDOCS', 'exec'), {})
            os.environ['GITHUB_REF'] = 'refs/heads/main'
            release['draft'] = True
            with self.assertRaises(SystemExit):
                exec(compile(program, 'docs.yml:PYDOCS', 'exec'), {})

    def test_release_notes_bind_coordinates_and_provenance(self):
        text = notes.render('0.1.11', 'a' * 40, ['- Fix'], 'x/y', True)
        for fragment in ('dev.usbdac:usbdac:0.1.11', '/releases/download/v0.1.11/manifest.json',
                         '/releases/download/v0.1.11/receipt.json', '/commit/' + 'a' * 40,
                         'usbdac-validator-0.1.11-macos-arm64.dmg'):
            self.assertIn(fragment, text)


if __name__ == '__main__':
    unittest.main()
