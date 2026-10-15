"""Deterministic platform adapter checks; native UI proof runs separately."""
from pathlib import Path, PurePosixPath
import subprocess
import json
import stat
from types import SimpleNamespace
import unittest
from unittest.mock import patch
from chat_ui_platform import NativeProcesses, fixture_environment, install_command, launch_command, windows_command_line, prepare_private_fixture

class PlatformTests(unittest.TestCase):
    def test_preserve_home_platform_runtime_and_exclude_account_secrets(self):
        inherited = {'HOME':'original','PATH':'tools','SystemRoot':'C:/Windows','ComSpec':'C:/Windows/System32/cmd.exe','TEMP':'owned-temp','TMP':'owned-tmp','DISPLAY':':99','GITHUB_TOKEN':'secret','OPENAI_API_KEY':'secret','AWS_PROFILE':'account'}
        for platform in ['darwin','linux','win32']:
            with self.subTest(platform=platform):
                env=fixture_environment(inherited,platform)
                self.assertEqual(env['HOME'],'original')
                self.assertFalse(any('secret' == value or 'account' == value for value in env.values()))
                self.assertEqual('SystemRoot' in env,platform=='win32')
                self.assertEqual('DISPLAY' in env,platform=='linux')

    def test_exact_native_launch_paths(self):
        root=PurePosixPath('/owned/host')
        self.assertEqual(launch_command(root,'darwin'),['/owned/host/Contents/MacOS/Code'])
        self.assertEqual(launch_command(root,'linux'),['/owned/host/code'])
        self.assertEqual(launch_command(root,'win32'),['/owned/host/Code.exe'])

    def test_darwin_and_linux_cli_preserve_argument_boundaries(self):
        args=['--user-data-dir','profile with spaces','--install-extension','candidate.vsix']
        for platform,script in [('darwin','Contents/Resources/app/bin/code'),('linux','bin/code')]:
            command=install_command(PurePosixPath('/owned/host'),args,{},platform)
            self.assertEqual(command,[f'/owned/host/{script}',*args])

    def test_windows_batch_shell_quotes_spaces_once_and_disables_autorun(self):
        command=install_command(PurePosixPath('C:/Program Files/Code'),['--user-data-dir','C:/owned profile'],{'COMSPEC':'cmd.exe'},'win32')
        self.assertEqual(command[:4],['cmd.exe','/d','/s','/c'])
        self.assertEqual(windows_command_line(command),'cmd.exe /d /s /c ""C:/Program Files/Code/bin/code.cmd" "--user-data-dir" "C:/owned profile""')
        self.assertNotIn('\\"',windows_command_line(command))
        with self.assertRaises(ValueError):install_command(PurePosixPath('C:/%UNOWNED%'),[],{'COMSPEC':'cmd.exe'},'win32')

    def test_windows_preserves_systemroot_fallback_without_comspec(self):
        command=install_command(Path('C:/Code'),[],{'SYSTEMROOT':'C:/Windows'},'win32')
        self.assertEqual(command[0],str(Path('C:/Windows')/'System32/cmd.exe'))

    def test_unix_start_has_new_owned_session_and_exact_tree_stop(self):
        with patch('chat_ui_platform.sys.platform','linux'), patch('chat_ui_platform.subprocess.Popen') as popen, patch('chat_ui_platform.os.killpg') as kill:
            owner=NativeProcesses()
            child=owner.start(['owned-host'],{'HOME':'original'},'owned-log')
            popen.assert_called_once_with(['owned-host'],env={'HOME':'original'},stdout='owned-log',stderr=subprocess.STDOUT,start_new_session=True)
            owner.stop(child)
            self.assertEqual(kill.call_args.args[0],child.pid)

    def test_unix_natural_descendant_exit_during_grace_does_not_force(self):
        probes = 0
        def kill(group, signal):
            nonlocal probes
            self.assertEqual((group, signal), (42, 0))
            probes += 1
            if probes > 1: raise ProcessLookupError()
        with patch('chat_ui_platform.sys.platform','linux'), patch('chat_ui_platform.os.killpg',side_effect=kill), patch('chat_ui_platform.time.monotonic',return_value=0), patch('chat_ui_platform.time.sleep') as sleep:
            owner=NativeProcesses()
            owner.groups=[42]
            result=owner.close()
            self.assertFalse(result['forced_descendants'])
            self.assertEqual(result['members_remaining'],0)
            sleep.assert_called_once_with(.01)

    def test_unix_expired_grace_forces_and_observes_zero(self):
        dead = False
        def kill(group, received):
            nonlocal dead
            self.assertEqual(group,42)
            if received:
                dead=True
            elif dead:
                raise ProcessLookupError()
        with patch('chat_ui_platform.sys.platform','linux'), patch('chat_ui_platform.os.killpg',side_effect=kill), patch('chat_ui_platform.time.monotonic',side_effect=[0,6,6]):
            owner=NativeProcesses()
            owner.groups=[42]
            result=owner.close()
            self.assertTrue(result['forced_descendants'])
            self.assertEqual(result['forced_groups'],[42])
            self.assertEqual(result['members_remaining'],0)

    def test_private_initialization_is_noop_on_existing_unix_platforms(self):
        with patch('chat_ui_platform.subprocess.run') as run, patch('chat_ui_platform.Path.lstat') as metadata:
            for platform in ['darwin','linux']:
                self.assertIsNone(prepare_private_fixture(Path('/owned'),{},platform))
            run.assert_not_called()
            metadata.assert_not_called()

    def test_windows_private_initialization_confines_paths_and_strips_secrets(self):
        root=Path('C:/owned fixture')
        observed=[]
        def metadata(path):
            observed.append(path.relative_to(root).as_posix())
            isfile=path.name=='config.toml'
            return SimpleNamespace(st_mode=stat.S_IFREG if isfile else stat.S_IFDIR,st_file_attributes=0,st_nlink=1)
        rows=[{'relative':name,'owner':'S-1-5-21-fixture','protected':True} for name in ['', 'config', 'data', 'local-data', 'state', 'cache', 'credentials', 'config/config.toml']]
        with patch('chat_ui_platform.Path.lstat',metadata), patch('chat_ui_platform.subprocess.run',return_value=SimpleNamespace(stdout=json.dumps(rows))) as run:
            self.assertEqual(prepare_private_fixture(root,{'SystemRoot':'C:/Windows','HOME':'original','OPENAI_API_KEY':'secret'},'win32'),rows)
        self.assertEqual(observed,['.', 'config', 'data', 'local-data', 'state', 'cache', 'credentials', 'config/config.toml'])
        command=run.call_args.args[0]
        self.assertTrue(command[0].endswith('WindowsPowerShell/v1.0/powershell.exe'))
        self.assertEqual(command[1:5],['-NoLogo','-NoProfile','-NonInteractive','-Command'])
        self.assertEqual(run.call_args.kwargs['env'],{'HOME':'original','SystemRoot':'C:/Windows','MINI_AGENT_CHAT_PRIVATE_ROOT':str(root)})
        self.assertTrue(run.call_args.kwargs['check'])
        self.assertEqual(run.call_args.kwargs['timeout'],30)

    def test_windows_private_initialization_rejects_link_before_mutation(self):
        metadata=SimpleNamespace(st_mode=stat.S_IFDIR,st_file_attributes=0x400,st_nlink=1)
        with patch('chat_ui_platform.Path.lstat',return_value=metadata), patch('chat_ui_platform.subprocess.run') as run:
            with self.assertRaisesRegex(ValueError,'reparse'):
                prepare_private_fixture(Path('C:/owned'),{'SystemRoot':'C:/Windows'},'win32')
            run.assert_not_called()

    def test_windows_uses_job_for_start_stop_and_retirement(self):
        with patch('chat_ui_platform.sys.platform','win32'), patch('chat_ui_platform._WindowsJob') as job:
            owner=NativeProcesses()
            self.assertIs(owner.start(['Code.exe'],{},'log'),job.return_value.start.return_value)
            owner.stop(None)
            job.return_value.terminate.assert_called_once_with()
            self.assertIs(owner.close(),job.return_value.retire.return_value)

if __name__=='__main__':unittest.main()
