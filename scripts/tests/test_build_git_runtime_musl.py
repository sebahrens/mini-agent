import importlib.util
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location('musl', Path(__file__).resolve().parents[1] / 'build-git-runtime-musl.py')
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class MuslBuildTests(unittest.TestCase):
    def setUp(self):
        owner = tempfile.TemporaryDirectory(prefix='musl-launcher-test-')
        self.addCleanup(owner.cleanup)
        self.root = Path(owner.name).resolve()
        context = self.root / 'scripts/git-runtime'
        context.mkdir(parents=True)
        archive = b'fixture curl archive'
        (context / 'curl-8.23.0.tar.xz').write_bytes(archive)
        (context / 'Dockerfile.musl').write_text('fixture recipe')
        self.enterContext(patch.object(MODULE, 'CURL_SHA256', MODULE.hashlib.sha256(archive).hexdigest()))
        self.output = self.root / 'target/bundle'

    def test_invalid_target_jobs_and_source_never_start_docker(self):
        with patch.object(MODULE, 'run') as run:
            for target,jobs in [('x86_64-unknown-linux-gnu',1),('aarch64-unknown-linux-musl',0)]:
                with self.assertRaises(ValueError):
                    MODULE.build(root=self.root, output=self.output, target=target, jobs=jobs)
            (self.root / 'scripts/git-runtime/curl-8.23.0.tar.xz').write_bytes(b'changed')
            with self.assertRaisesRegex(ValueError, 'hash mismatch'):
                MODULE.build(root=self.root, output=self.output, target='aarch64-unknown-linux-musl')
        run.assert_not_called()

    def test_existing_output_is_preserved(self):
        self.output.mkdir(parents=True)
        (self.output/'sentinel').write_text('keep')
        with patch.object(MODULE, 'run') as run, self.assertRaises(ValueError):
            MODULE.build(root=self.root, output=self.output, target='aarch64-unknown-linux-musl')
        run.assert_not_called()
        self.assertEqual((self.output/'sentinel').read_text(),'keep')

    def test_actual_platform_and_readonly_source_are_explicit(self):
        commands=[]
        def run(command, timeout):
            commands.append(command)
            if command[1]=='run':
                self.output.mkdir()
                (self.output/'bundle.manifest').write_text('published fixture')
        with patch.object(MODULE,'run',side_effect=run):
            MODULE.build(root=self.root,output=self.output,target='aarch64-unknown-linux-musl')
        for command in commands:
            self.assertEqual(command[command.index('--platform')+1],'linux/arm64')
        execute=commands[1]
        self.assertIn(f'type=bind,src={self.root},dst=/source,readonly',execute)
        self.assertEqual(execute[execute.index('--network')+1],'none')
        self.assertIn('--read-only',execute)
        self.assertIn('ALL',execute)
        self.assertNotIn('--privileged',execute)
        self.assertEqual((self.root/'target/git-runtime-musl-cache').stat().st_mode & 0o777,0o700)

    def test_failed_owned_container_is_retired_without_touching_output(self):
        commands=[]
        def run(command,timeout):
            commands.append(command)
            if command[1]=='run':raise subprocess.TimeoutExpired(command,timeout)
        with patch.object(MODULE,'run',side_effect=run),patch.object(MODULE.subprocess,'run') as retire,self.assertRaises(subprocess.TimeoutExpired):
            MODULE.build(root=self.root,output=self.output,target='x86_64-unknown-linux-musl')
        name=commands[-1][commands[-1].index('--name')+1]
        self.assertTrue(name.startswith('mini-agent-git-musl-'))
        self.assertEqual(retire.call_args.args[0],['docker','rm','--force',name])
        self.assertFalse(self.output.exists())


    def test_sources_use_separate_public_network_and_exact_readonly_bundle(self):
        self.output.mkdir(parents=True)
        (self.output / 'bundle.manifest').write_text('fixture')
        sources = self.root / 'native-sources'
        commands = []
        def run(command, timeout):
            commands.append(command)
            sources.mkdir()
            (sources / 'native-components.json').write_text('fixture record')
        with patch.object(MODULE, 'run', side_effect=run):
            record = MODULE.collect_sources(root=self.root, target='aarch64-unknown-linux-musl', bundle=self.output, output=sources)
        self.assertEqual(record, sources / 'native-components.json')
        command, = commands
        self.assertEqual(command[command.index('--network') + 1], 'bridge')
        self.assertIn(f'type=bind,src={self.output},dst=/bundle,readonly', command)
        self.assertIn(f'type=bind,src={self.root},dst=/source,readonly', command)
        self.assertIn('--read-only', command)
        self.assertIn('--cap-drop', command)
        self.assertNotIn('--privileged', command)
        self.assertNotIn('--env', command)
        self.assertNotIn('--env-file', command)
        self.assertEqual(command[command.index('--curl-source') + 1], '/source/scripts/git-runtime/curl-8.23.0.tar.xz')
        self.assertIn('--static-curl-closure', command)
        self.assertNotIn('--package', command)
        with patch.object(MODULE, 'run') as again, self.assertRaises(ValueError):
            MODULE.collect_sources(root=self.root, target='aarch64-unknown-linux-musl', bundle=self.output, output=sources)
        again.assert_not_called()

    def test_failed_source_container_retires_exact_actor_and_preserves_bundle(self):
        self.output.mkdir(parents=True)
        manifest = self.output / 'bundle.manifest'
        manifest.write_text('keep built data')
        commands = []
        def run(command, timeout):
            commands.append(command)
            raise subprocess.TimeoutExpired(command, timeout)
        with patch.object(MODULE, 'run', side_effect=run), patch.object(MODULE.subprocess, 'run') as retire, self.assertRaises(subprocess.TimeoutExpired):
            MODULE.collect_sources(root=self.root, target='x86_64-unknown-linux-musl', bundle=self.output, output=self.root / 'native-sources')
        command, = commands
        name = command[command.index('--name') + 1]
        self.assertTrue(name.startswith('mini-agent-git-sources-'))
        self.assertEqual(retire.call_args.args[0], ['docker', 'rm', '--force', name])
        self.assertEqual(manifest.read_text(), 'keep built data')


if __name__=='__main__':unittest.main()
