#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = ["PyYAML==6.0.3"]
# ///
"""Exercise the actual release workflow scripts without GitHub API calls.

Run with `uv run scripts/test-actions.py`. All test files stay under tmp/.
"""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

import yaml

ROOT = Path(__file__).resolve().parent.parent
WORKFLOW = yaml.safe_load((ROOT / '.github/workflows/release.yml').read_text())
VERSION = next(step['run'] for step in WORKFLOW['jobs']['version']['steps']
               if step.get('id') == 'version')
PUBLISH = next(step['run'] for step in WORKFLOW['jobs']['publish']['steps']
               if step.get('name') == 'Publish GitHub release')


class WorkflowTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        (ROOT / 'tmp').mkdir(exist_ok=True)

    def run_version(self, version, tag):
        with tempfile.TemporaryDirectory(dir=ROOT / 'tmp') as tmp:
            cwd = Path(tmp)
            (cwd / 'Cargo.toml').write_text(f'[workspace.package]\nversion = "{version}"\n')
            output = cwd / 'output'
            env = dict(os.environ, RELEASE_TAG=tag, GITHUB_OUTPUT=str(output))
            result = subprocess.run(['bash', '-euo', 'pipefail', '-c', VERSION],
                                    cwd=cwd, env=env, capture_output=True, text=True)
            return result, output.read_text() if output.exists() else ''

    def test_valid_version(self):
        result, output = self.run_version('0.1.0', 'v0.1.0')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(output, 'prerelease=false\n')

    def test_prerelease(self):
        result, output = self.run_version('1.0.0-rc.1', 'v1.0.0-rc.1')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(output, 'prerelease=true\n')

    def test_build_metadata_is_not_prerelease(self):
        result, output = self.run_version('1.0.0+build-1', 'v1.0.0+build-1')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(output, 'prerelease=false\n')

    def test_reject_wrong_tag(self):
        for tag in ['v0.2.0', '0.1.0', 'v0.1.0-rc.1', 'v0.1.0\n', 'v$(exit 0)']:
            with self.subTest(tag=tag):
                result, output = self.run_version('0.1.0', tag)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('Release tag must be v0.1.0', result.stderr)
                self.assertEqual(output, '')

    def run_publish(self, state, upload_fails=False, prerelease=False):
        with tempfile.TemporaryDirectory(dir=ROOT / 'tmp') as tmp:
            cwd = Path(tmp)
            mock = cwd / 'mock'
            mock.mkdir()
            gh = mock / 'gh'
            gh.write_text('''#!/usr/bin/env python3
import json, os, sys
with open(os.environ['MOCK_LOG'], 'a') as log:
    log.write(json.dumps(sys.argv[1:]) + '\\n')
command = sys.argv[2]
if command == 'view':
    state = os.environ['MOCK_STATE']
    if state == 'absent': sys.exit(1)
    print('true' if state == 'draft' else 'false')
if command == 'upload' and os.environ['MOCK_UPLOAD_FAILS'] == '1': sys.exit(1)
''')
            gh.chmod(0o755)
            (cwd / 'bin').mkdir()
            for name in ['bin/gpudog', 'bin/gpudogd', 'gpudog.tar.gz', 'gpudog.deb',
                         'SHA256SUMS', 'dependencies.txt', 'toolchain.txt']:
                (cwd / name).touch()
            log = cwd / 'log'
            notes = cwd / 'release-notes.md'
            notes.write_text('notes\n')
            env = dict(os.environ, PATH=str(mock) + os.pathsep + os.environ['PATH'],
                       MOCK_LOG=str(log), MOCK_STATE=state, NOTES_FILE=str(notes),
                       MOCK_UPLOAD_FAILS=str(int(upload_fails)),
                       RELEASE_TAG='v0.1.0', PRERELEASE=str(prerelease).lower())
            result = subprocess.run(['bash', '-euo', 'pipefail', '-c', PUBLISH],
                                    cwd=cwd, env=env, capture_output=True, text=True)
            return result, [json.loads(line) for line in log.read_text().splitlines()]

    def test_new_release_uses_draft(self):
        result, calls = self.run_publish('absent')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([call[1] for call in calls], ['view', 'create', 'upload', 'edit'])
        self.assertIn('--draft', calls[1])
        self.assertIn('--verify-tag', calls[1])
        self.assertTrue(calls[1][calls[1].index('--notes-file') + 1].endswith('/release-notes.md'))
        self.assertNotIn('--notes', calls[1])
        self.assertIn('--prerelease=false', calls[-1])
        self.assertIn('./gpudog.tar.gz', calls[2])
        self.assertIn('./gpudog.deb', calls[2])
        self.assertNotIn('bin/gpudog', calls[2])
        self.assertNotIn('bin/gpudogd', calls[2])
        self.assertIn('SHA256SUMS', calls[2])

    def test_resume_draft(self):
        result, calls = self.run_publish('draft')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([call[1] for call in calls], ['view', 'upload', 'edit'])
        self.assertIn('--clobber', calls[1])

    def test_published_release_never_overwritten(self):
        result, calls = self.run_publish('published')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual([call[1] for call in calls], ['view'])

    def test_upload_failure_never_publishes(self):
        result, calls = self.run_publish('absent', upload_fails=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual([call[1] for call in calls], ['view', 'create', 'upload'])

    def test_prerelease_publication(self):
        result, calls = self.run_publish('absent', prerelease=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('--prerelease=true', calls[-1])


NOTES_STEP = next(step['run'] for step in WORKFLOW['jobs']['version']['steps']
                  if step.get('name') == 'Write release notes from CHANGELOG.md')

CHANGELOG = """# Changelog

## Compatibility

No promise.

## [0.2.0] - Unreleased

### Added

*   Next.

## [0.1.0] - 2026-11-10

### Added

*   First.
"""


class ReleaseNotesTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        # unittest runs classes by name, so this one may run first.
        (ROOT / 'tmp').mkdir(exist_ok=True)

    def run_notes(self, tag, changelog=CHANGELOG):
        with tempfile.TemporaryDirectory(dir=ROOT / 'tmp') as tmp:
            cwd = Path(tmp)
            (cwd / 'scripts').mkdir()
            (cwd / 'scripts/release-notes.py').write_bytes(
                (ROOT / 'scripts/release-notes.py').read_bytes())
            (cwd / 'CHANGELOG.md').write_text(changelog)
            env = dict(os.environ, RELEASE_TAG=tag)
            result = subprocess.run(['bash', '-euo', 'pipefail', '-c', NOTES_STEP],
                                    cwd=cwd, env=env, capture_output=True, text=True)
            notes = cwd / 'release-notes.md'
            return result, notes.read_text() if notes.exists() else ''

    def test_release_notes_hold_compatibility_and_the_version(self):
        result, notes = self.run_notes('v0.1.0')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('## Compatibility\n\nNo promise.', notes)
        self.assertIn('## [0.1.0] - 2026-11-10\n\n### Added\n\n*   First.', notes)
        self.assertNotIn('Next.', notes)
        self.assertIn('this is not real GPU validation', notes)

    def test_an_unreleased_version_stops_the_release(self):
        result, _ = self.run_notes('v0.2.0')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("no '## [0.2.0] - YYYY-MM-DD' section", result.stderr)

    def test_a_prerelease_may_use_its_unreleased_version(self):
        result, notes = self.run_notes('v0.2.0-rc.1')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('## [0.2.0] - Unreleased', notes)

    def test_a_missing_compatibility_section_stops_the_release(self):
        result, _ = self.run_notes('v0.1.0', CHANGELOG.replace('## Compatibility', '## Other'))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('Compatibility', result.stderr)

    def test_an_unreleased_version_says_to_date_it(self):
        result, _ = self.run_notes('v0.2.0')
        self.assertIn('replace Unreleased with the release date', result.stderr)
        result, _ = self.run_notes('v0.3.0')
        self.assertIn('add the section', result.stderr)

    def test_build_metadata_keeps_the_prerelease_base(self):
        result, notes = self.run_notes('v0.2.0-rc.1+b-1')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('## [0.2.0] - Unreleased', notes)

    def test_publish_reads_the_notes_the_version_job_uploads(self):
        steps = WORKFLOW['jobs']['version']['steps']
        upload = next(step for step in steps if 'upload-artifact' in step.get('uses', ''))
        download = next(step for step in WORKFLOW['jobs']['publish']['steps']
                        if step.get('with', {}).get('name', '').startswith('release-notes-'))
        publish = next(step for step in WORKFLOW['jobs']['publish']['steps']
                       if step.get('name') == 'Publish GitHub release')
        self.assertEqual(upload['with']['name'], download['with']['name'])
        self.assertIn('> ' + upload['with']['path'], NOTES_STEP)
        self.assertEqual(publish['env']['NOTES_FILE'],
                         '${{ github.workspace }}/' + download['with']['path'] + '/'
                         + upload['with']['path'])

    def test_the_repository_changelog_has_the_sections(self):
        # A prerelease, because only a prerelease may use the Unreleased
        # [0.0.1] section; once 0.0.1 is dated, any version with a section works.
        result = subprocess.run(['python3', str(ROOT / 'scripts/release-notes.py'), '0.0.1-rc.1'],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == '__main__':
    unittest.main(verbosity=2)
