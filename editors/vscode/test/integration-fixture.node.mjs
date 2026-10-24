import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { linkSync, lstatSync, mkdirSync, mkdtempSync, readFileSync, renameSync, rmSync, symlinkSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, win32 } from 'node:path';
import test from 'node:test';
import { createIntegrationFixture, fixturePowerShellEnvironment, WINDOWS_FIXTURE_MODULE_SETUP } from '../scripts/integration-fixture.mjs';

function base(t) {
  const path = mkdtempSync(join(tmpdir(), 'ma-fixture-test-'));
  t.after(() => rmSync(path, { recursive: true, force: true }));
  return path;
}

function successfulInitialization() {
  return { status: 0, stdout: JSON.stringify({ readback: true, after: [{ relative: 'config.toml' }, { relative: '.' }] }) };
}

test('exclusive fixture config is readable and cannot be overwritten', t => {
  const fixture = createIntegrationFixture(base(t), { platform: 'linux', run: () => assert.fail('Unix must not invoke PowerShell') });
  fixture.writeConfig('provider = "fixture"\n');
  assert.equal(readFileSync(join(fixture.configRoot, 'config.toml'), 'utf8'), 'provider = "fixture"\n');
  if (process.platform !== 'win32') {
    assert.equal(lstatSync(fixture.configRoot).mode & 0o777, 0o700);
    assert.equal(lstatSync(join(fixture.configRoot, 'config.toml')).mode & 0o777, 0o600);
  }
  assert.throws(() => fixture.writeConfig('replacement'), /EEXIST/);
});

test('Windows initializer uses canonical PowerShell, bounded transport and no inherited secrets', t => {
  let invoked = false;
  const fixture = createIntegrationFixture(base(t), {
    platform: 'win32', environment: { SystemRoot: 'C:\\Windows', TEMP: 'C:\\Temp', USERPROFILE: 'C:\\Users\\fixture', Path: 'C:\\Windows\\System32', OPENAI_API_KEY: 'must-not-be-forwarded', PSModulePath: 'incompatible PS7 modules' },
    run(program, args, options) {
      invoked = true;
      assert.equal(program, 'C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe');
      assert.deepEqual(args.slice(0, 4), ['-NoLogo', '-NoProfile', '-NonInteractive', '-EncodedCommand']);
      const script = Buffer.from(args[4], 'base64').toString('utf16le');
      assert.ok(script.includes('Set-Acl -LiteralPath'));
      assert.ok(!/\[Console\]::(?:OutputEncoding|InputEncoding)|Text\.(?:UTF8|Unicode|ASCII)Encoding/i.test(script), 'ASCII fixture diagnostics must not mutate console encoding');
      assert.ok(script.indexOf("Trace-Stage 'transport'") < script.indexOf("Trace-Stage 'current-user'"));
      assert.ok(script.includes(WINDOWS_FIXTURE_MODULE_SETUP));
      assert.ok(script.indexOf(WINDOWS_FIXTURE_MODULE_SETUP) < script.indexOf('New-Object'));
      assert.ok(script.indexOf(WINDOWS_FIXTURE_MODULE_SETUP) < script.indexOf('Join-Path'));
      assert.ok(!script.includes(fixture.root), 'paths must be transported through env, not script interpolation');
      assert.deepEqual(options.env, { SystemRoot: 'C:\\Windows', TEMP: 'C:\\Temp', USERPROFILE: 'C:\\Users\\fixture', PATH: 'C:\\Windows\\System32', MINI_AGENT_VSIX_PRIVATE_ROOT: fixture.configRoot });
      assert.equal(options.timeout, 30_000);
      assert.equal(options.maxBuffer, 64 * 1024);
      assert.equal(options.input, '');
      return successfulInitialization();
    },
  });
  assert.equal(fixture.writeConfig('offline fixture').readback, true);
  assert.equal(invoked, true);
});

test('module setup pins system manifests before cmdlet use and does not discover ambient modules', () => {
  assert.ok(WINDOWS_FIXTURE_MODULE_SETUP.includes("$fixtureModules = [IO.Path]::Combine($PSHOME, 'Modules')"));
  assert.ok(WINDOWS_FIXTURE_MODULE_SETUP.includes('$env:PSModulePath = $fixtureModules'));
  assert.ok(WINDOWS_FIXTURE_MODULE_SETUP.includes("$PSModuleAutoLoadingPreference = 'None'"));
  assert.ok(WINDOWS_FIXTURE_MODULE_SETUP.includes("@('Microsoft.PowerShell.Utility', 'Microsoft.PowerShell.Management', 'Microsoft.PowerShell.Security')"));
  assert.ok(WINDOWS_FIXTURE_MODULE_SETUP.includes("[IO.Path]::Combine([IO.Path]::Combine($fixtureModules, $module), $module + '.psd1')"));
  assert.ok(WINDOWS_FIXTURE_MODULE_SETUP.indexOf('$env:PSModulePath =') < WINDOWS_FIXTURE_MODULE_SETUP.indexOf('Import-Module -Name $manifest -ErrorAction Stop'));
  assert.ok(!/New-Object|Join-Path|Get-Module|Get-Command|USERPROFILE|ProgramFiles/.test(WINDOWS_FIXTURE_MODULE_SETUP));
});

test('PowerShell environment retains OS/profile/temp inputs with Windows case-insensitive names', () => {
  const inherited = {
    SYSTEMROOT: 'C:\\Windows', windir: 'C:\\Windows', COMSPEC: 'C:\\Windows\\System32\\cmd.exe',
    TEMP: 'C:\\Temp', TMP: 'C:\\Temp2', USERPROFILE: 'C:\\Users\\fixture', APPDATA: 'C:\\Users\\fixture\\AppData\\Roaming',
    LOCALAPPDATA: 'C:\\Users\\fixture\\AppData\\Local', Path: 'C:\\Windows\\System32', PATHEXT: '.EXE;.CMD',
    HOME: 'fixture-home', USER: 'fixture-user', LOGNAME: 'fixture-user', TMPDIR: 'fixture-temp', SHELL: 'fixture-shell', LANG: 'en_US.UTF-8',
    OPENROUTER_API_KEY: 'secret', MINI_AGENT_HOME: 'operator-config', PSModulePath: 'incompatible PS7 modules',
  };
  assert.deepEqual(fixturePowerShellEnvironment(inherited), {
    SystemRoot: inherited.SYSTEMROOT, WINDIR: inherited.windir, ComSpec: inherited.COMSPEC,
    TEMP: inherited.TEMP, TMP: inherited.TMP, USERPROFILE: inherited.USERPROFILE, APPDATA: inherited.APPDATA,
    LOCALAPPDATA: inherited.LOCALAPPDATA, PATH: inherited.Path, PATHEXT: inherited.PATHEXT,
    HOME: inherited.HOME, USER: inherited.USER, LOGNAME: inherited.LOGNAME, TMPDIR: inherited.TMPDIR, SHELL: inherited.SHELL, LANG: inherited.LANG,
  });
});

test('timeouts and protocol errors retain bounded sanitized stdout/stderr stage evidence', t => {
  const content = 'fixture-private-content';
  for (const outcome of [
    { status: null, error: Object.assign(new Error('spawn timed out'), { code: 'ETIMEDOUT' }), stdout: `partial-json ${content}`, stderr: `fixture-private-init:transport\nfixture-private-init:before-acl:config.toml\n${content}\u0000${'x'.repeat(12_000)}` },
    { status: 1, stdout: 'partial-json', stderr: 'fixture-private-init:error-hresult:-2147024891' },
    { status: 0, stdout: 'partial-json', stderr: 'fixture-private-init:complete' },
    { status: 0, stdout: '{}', stderr: 'fixture-private-init:readback' },
    { status: 0, stdout: 'null', stderr: 'fixture-private-init:readback' },
  ]) {
    const fixture = createIntegrationFixture(base(t), { platform: 'win32', environment: { SystemRoot: 'C:\\Windows' }, run: () => outcome });
    assert.throws(() => fixture.writeConfig(content), error => {
      assert.match(error.message, /stdout=/);
      assert.match(error.message, /stderr=fixture-private-init:/);
      assert.ok(!error.message.includes(content));
      assert.ok(!error.message.includes('\u0000'));
      assert.ok(error.message.length < 16_600);
      if (outcome.error) {
        assert.match(error.message, /ETIMEDOUT/);
        assert.match(error.message, /before-acl:config.toml/);
        assert.match(error.message, /stdout=partial-json/);
      }
      return true;
    });
  }
});

test('initializer rejects replaced or linked directories before invoking PowerShell', t => {
  for (const kind of ['replacement', 'junction']) {
    const fixture = createIntegrationFixture(base(t), {
      platform: 'win32', environment: { SystemRoot: 'C:\\Windows' }, run: () => assert.fail('unadmitted fixture reached PowerShell'),
    });
    const original = `${fixture.configRoot}-original`;
    renameSync(fixture.configRoot, original);
    if (kind === 'replacement') { mkdirSync(fixture.configRoot); }
    else { symlinkSync(original, fixture.configRoot, process.platform === 'win32' ? 'junction' : 'dir'); }
    assert.throws(() => fixture.writeConfig('fixture'), /fixture (identity changed|path)/);
  }
});

test('initializer never adopts an existing file, hardlink, or wrong-type leaf', t => {
  for (const kind of ['file', 'hardlink', 'directory']) {
    const fixture = createIntegrationFixture(base(t), {
      platform: 'win32', environment: { SystemRoot: 'C:\\Windows' }, run: () => assert.fail('existing leaf reached PowerShell'),
    });
    const config = join(fixture.configRoot, 'config.toml');
    if (kind === 'directory') { mkdirSync(config); }
    else if (kind === 'file') { writeFileSync(config, 'existing'); }
    else {
      const other = join(fixture.root, 'other');
      writeFileSync(other, 'existing');
      linkSync(other, config);
    }
    assert.throws(() => fixture.writeConfig('replacement'), /EEXIST/);
    if (kind !== 'directory') { assert.equal(readFileSync(config, 'utf8'), 'existing'); }
  }
});

test('Windows transport errors and incomplete initialization fail closed', t => {
  for (const outcome of [
    { status: 1, stderr: 'fixture ACL failure' },
    { status: null, error: new Error('spawn timed out') },
    { status: 0, stdout: '{}' },
    { status: 0, stdout: 'not JSON' },
  ]) {
    const fixture = createIntegrationFixture(base(t), {
      platform: 'win32', environment: { SystemRoot: 'C:\\Windows' }, run: () => outcome,
    });
    assert.throws(() => fixture.writeConfig('fixture'));
  }
  for (const environment of [{}, { SystemRoot: 'relative' }]) {
    const fixture = createIntegrationFixture(base(t), { platform: 'win32', environment, run: () => assert.fail('invalid SystemRoot reached spawn') });
    assert.throws(() => fixture.writeConfig('fixture'), /SystemRoot/);
  }
});

test('post-initialization admission rejects replaced, hardlinked or modified config', t => {
  for (const kind of ['replacement', 'hardlink', 'content']) {
    const fixture = createIntegrationFixture(base(t), {
      platform: 'win32', environment: { SystemRoot: 'C:\\Windows' }, run() {
        const config = join(fixture.configRoot, 'config.toml');
        if (kind === 'replacement') { renameSync(config, `${config}-old`); writeFileSync(config, 'fixture'); }
        else if (kind === 'hardlink') { linkSync(config, join(fixture.root, 'linked')); }
        else { writeFileSync(config, 'modified'); }
        return successfulInitialization();
      },
    });
    assert.throws(() => fixture.writeConfig('fixture'), /fixture/);
  }
});

// No Cargo/npm dependencies: run this on Windows against the actual native ACL
// implementation before building release artifacts. The inherited-only control
// records the host's behavior; it does not assume the historical failure cause.
test('native Windows protected config survives parent repair; inherited control retains evidence', { skip: process.platform !== 'win32' }, t => {
  const fixture = createIntegrationFixture(base(t));
  const initialized = fixture.writeConfig('offline fixture\n');
  assert.equal(initialized.readback, true);
  for (const row of initialized.after) {
    assert.equal(row.ownerCurrentUser, true);
    assert.equal(row.protected, true);
    assert.deepEqual(row.rules.map(rule => rule.trustee).sort(), ['SYSTEM', 'current-user']);
    for (const rule of row.rules) {
      assert.equal(rule.inherited, false);
      assert.equal(rule.inheritance, 0);
      assert.equal(rule.propagation, 0);
      assert.equal(rule.allow, true);
      assert.equal(rule.fullControl, true);
    }
  }
  t.diagnostic(`initialized ACLs/readback: ${JSON.stringify(initialized)}`);
  const script = String.raw`
$ErrorActionPreference = 'Stop'
function Trace-Stage($stage) { [Console]::Error.WriteLine('fixture-private-control:' + $stage) }
Trace-Stage 'transport'
try {
${WINDOWS_FIXTURE_MODULE_SETUP}
$user = [Security.Principal.WindowsIdentity]::GetCurrent().User
$system = New-Object Security.Principal.SecurityIdentifier('S-1-5-18')
function Set-OwnedAcl($path, $directory, $inherit, $setOwner = $true) {
    [Console]::Error.WriteLine('fixture-private-control:descriptor')
    $acl = if ($directory) { New-Object Security.AccessControl.DirectorySecurity } else { New-Object Security.AccessControl.FileSecurity }
    if ($setOwner) { $acl.SetOwner($user) }
    $acl.SetAccessRuleProtection($true, $false)
    $flags = if ($inherit) { [Security.AccessControl.InheritanceFlags]::ContainerInherit -bor [Security.AccessControl.InheritanceFlags]::ObjectInherit } else { [Security.AccessControl.InheritanceFlags]::None }
    foreach ($sid in @($user, $system)) {
        [void]$acl.AddAccessRule((New-Object Security.AccessControl.FileSystemAccessRule($sid, [Security.AccessControl.FileSystemRights]::FullControl, $flags, [Security.AccessControl.PropagationFlags]::None, [Security.AccessControl.AccessControlType]::Allow)))
    }
    [Console]::Error.WriteLine('fixture-private-control:set-acl')
    Set-Acl -LiteralPath $path -AclObject $acl
}
$control = Join-Path $env:MINI_AGENT_FIXTURE_TEST_ROOT 'control'
$file = Join-Path $control 'config.toml'
[void][IO.Directory]::CreateDirectory($control)
Set-OwnedAcl $control $true $true
[IO.File]::WriteAllText($file, 'inherited control')
# Make ownership explicit while inherited FullControl is still available. The
# later cleanup only needs the owner's implicit WRITE_DAC, not WRITE_OWNER.
$ownerAcl = Get-Acl -LiteralPath $file
$ownerAcl.SetOwner($user)
Set-Acl -LiteralPath $file -AclObject $ownerAcl
[Console]::Error.WriteLine('fixture-private-control:before-acl')
$before = Get-Acl -LiteralPath $file
$rules = @($before.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier]))
$inheritedOnly = -not $before.AreAccessRulesProtected -and $rules.Count -eq 2 -and @($rules | Where-Object { -not $_.IsInherited }).Count -eq 0
Set-OwnedAcl $control $true $false
[Console]::Error.WriteLine('fixture-private-control:control-readback')
$controlReadable = $false
$controlError = $null
try { $controlReadable = [IO.File]::ReadAllText($file) -eq 'inherited control' } catch { $controlError = $_.Exception.GetBaseException().HResult }
$afterProtected = $null
$afterRuleCount = $null
try { $after = Get-Acl -LiteralPath $file; $afterProtected = $after.AreAccessRulesProtected; $afterRuleCount = @($after.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier])).Count } catch {}
# Restore the control for deterministic fixture cleanup, then repeat the same
# parent repair against the initialized protected leaf.
Set-OwnedAcl $file $false $false $false
Set-OwnedAcl $env:MINI_AGENT_VSIX_PRIVATE_ROOT $true $false
[Console]::Error.WriteLine('fixture-private-control:protected-readback')
$protectedReadable = [IO.File]::ReadAllText((Join-Path $env:MINI_AGENT_VSIX_PRIVATE_ROOT 'config.toml')) -eq ('offline fixture' + [char]10)
@{ controlInheritedOnlyBefore = $inheritedOnly; controlReadableAfter = $controlReadable; controlReadHResult = $controlError; controlProtectedAfter = $afterProtected; controlRuleCountAfter = $afterRuleCount; protectedReadableAfter = $protectedReadable } | ConvertTo-Json -Compress
[Console]::Error.WriteLine('fixture-private-control:complete')
} catch {
    [Console]::Error.WriteLine('fixture-private-control:error-hresult:' + $_.Exception.GetBaseException().HResult)
    exit 1
}
`;
  const launchEnvironment = fixturePowerShellEnvironment(process.env);
  const result = spawnSync(win32.join(launchEnvironment.SystemRoot, 'System32', 'WindowsPowerShell', 'v1.0', 'powershell.exe'), [
    '-NoLogo', '-NoProfile', '-NonInteractive', '-EncodedCommand', Buffer.from(script, 'utf16le').toString('base64'),
  ], {
    env: { ...launchEnvironment, MINI_AGENT_FIXTURE_TEST_ROOT: fixture.root, MINI_AGENT_VSIX_PRIVATE_ROOT: fixture.configRoot },
    input: '', encoding: 'utf8', timeout: 30_000, maxBuffer: 64 * 1024, windowsHide: true,
  });
  const captured = `error=${result.error?.code ?? 'none'}; stdout=${String(result.stdout ?? '').slice(0, 8192)}; stderr=${String(result.stderr ?? '').slice(0, 8192)}`;
  assert.equal(result.error, undefined, captured);
  assert.equal(result.status, 0, captured);
  const observed = JSON.parse(result.stdout.replace(/^\uFEFF/, ''));
  t.diagnostic(`native inherited/protected control: ${JSON.stringify(observed)}`);
  assert.equal(observed.controlInheritedOnlyBefore, true);
  assert.equal(observed.protectedReadableAfter, true);
  assert.equal(readFileSync(join(fixture.configRoot, 'config.toml'), 'utf8'), 'offline fixture\n');
});
