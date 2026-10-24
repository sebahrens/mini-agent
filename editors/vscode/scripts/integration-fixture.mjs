import { spawnSync } from 'node:child_process';
import { lstatSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { join, win32 } from 'node:path';

// Removing an inherited PS7 path still lets Windows PowerShell reconstruct
// CurrentUser/AllUsers discovery paths. This fixture only needs in-box modules:
// load their absolute manifests and disable ambient autoloading before the
// first Utility/Management/Security cmdlet. Import-Module itself is Core.
export const WINDOWS_FIXTURE_MODULE_SETUP = String.raw`
Trace-Stage 'module-path'
$fixtureModules = [IO.Path]::Combine($PSHOME, 'Modules')
$env:PSModulePath = $fixtureModules
$PSModuleAutoLoadingPreference = 'None'
foreach ($module in @('Microsoft.PowerShell.Utility', 'Microsoft.PowerShell.Management', 'Microsoft.PowerShell.Security')) {
    Trace-Stage ('import:' + $module)
    $manifest = [IO.Path]::Combine([IO.Path]::Combine($fixtureModules, $module), $module + '.psd1')
    Import-Module -Name $manifest -ErrorAction Stop
    Trace-Stage ('import-ready:' + $module)
}
`;

// This script only receives paths created by createIntegrationFixture below.
// Protect the leaf first: tightening a parent can remove an unprotected child's
// inherited grants before the child's own ACL can be initialized.
const WINDOWS_PRIVATE_FIXTURE = String.raw`
$ErrorActionPreference = 'Stop'
function Trace-Stage($stage) { [Console]::Error.WriteLine('fixture-private-init:' + $stage) }
Trace-Stage 'transport'
try {
# Stage names and normalized ACL fields are ASCII; changing console encoding
# is unnecessary and blocked before identity/ACL work on the hosted runner.
Trace-Stage 'current-user'
$user = [Security.Principal.WindowsIdentity]::GetCurrent().User
Trace-Stage 'current-user-ready'
${WINDOWS_FIXTURE_MODULE_SETUP}
Trace-Stage 'system-sid'
$system = New-Object Security.Principal.SecurityIdentifier('S-1-5-18')
Trace-Stage 'system-sid-ready'
function Inspect-Acl($path, $relative) {
    $acl = Get-Acl -LiteralPath $path
    $rules = @($acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier]))
    return @{
        relative = $relative
        ownerCurrentUser = $acl.GetOwner([Security.Principal.SecurityIdentifier]).Value -eq $user.Value
        protected = $acl.AreAccessRulesProtected
        rules = @($rules | ForEach-Object { @{
            trustee = if ($_.IdentityReference.Value -eq $user.Value) { 'current-user' } elseif ($_.IdentityReference.Value -eq $system.Value) { 'SYSTEM' } else { 'other' }
            inherited = $_.IsInherited
            inheritance = [int]$_.InheritanceFlags
            propagation = [int]$_.PropagationFlags
            allow = $_.AccessControlType -eq [Security.AccessControl.AccessControlType]::Allow
            fullControl = $_.FileSystemRights -eq [Security.AccessControl.FileSystemRights]::FullControl
        } })
    }
}
Trace-Stage 'fixture-paths'
$paths = @(
    @{ path = (Join-Path $env:MINI_AGENT_VSIX_PRIVATE_ROOT 'config.toml'); relative = 'config.toml'; directory = $false },
    @{ path = $env:MINI_AGENT_VSIX_PRIVATE_ROOT; relative = '.'; directory = $true }
)
Trace-Stage 'fixture-paths-ready'
$before = @()
foreach ($entry in $paths) {
    Trace-Stage ('inspect:' + $entry.relative)
    $item = Get-Item -LiteralPath $entry.path -Force
    if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -or $item.PSIsContainer -ne $entry.directory) { throw 'Invalid owned fixture path' }
    Trace-Stage ('before-acl:' + $entry.relative)
    $before += Inspect-Acl $entry.path $entry.relative
}
foreach ($entry in $paths) {
    Trace-Stage ('descriptor:' + $entry.relative)
    $acl = if ($entry.directory) { New-Object Security.AccessControl.DirectorySecurity } else { New-Object Security.AccessControl.FileSecurity }
    $acl.SetOwner($user)
    $acl.SetAccessRuleProtection($true, $false)
    foreach ($sid in @($user, $system)) {
        $rule = New-Object Security.AccessControl.FileSystemAccessRule($sid, [Security.AccessControl.FileSystemRights]::FullControl, [Security.AccessControl.InheritanceFlags]::None, [Security.AccessControl.PropagationFlags]::None, [Security.AccessControl.AccessControlType]::Allow)
        [void]$acl.AddAccessRule($rule)
    }
    Trace-Stage ('set-acl:' + $entry.relative)
    Set-Acl -LiteralPath $entry.path -AclObject $acl
}
$after = @()
foreach ($entry in $paths) {
    Trace-Stage ('verify:' + $entry.relative)
    $row = Inspect-Acl $entry.path $entry.relative
    if (-not $row.ownerCurrentUser -or -not $row.protected -or $row.rules.Count -ne 2) { throw 'Private fixture owner/DACL mismatch' }
    foreach ($rule in $row.rules) {
        if ($rule.trustee -notin @('current-user', 'SYSTEM') -or $rule.inherited -or $rule.inheritance -ne 0 -or $rule.propagation -ne 0 -or -not $rule.allow -or -not $rule.fullControl) { throw 'Private fixture ACL mismatch' }
    }
    if (@($row.rules | Where-Object { $_.trustee -eq 'current-user' }).Count -ne 1) { throw 'Private fixture trustees mismatch' }
    $after += $row
}
# Read after parent tightening, without emitting config contents or credentials.
Trace-Stage 'readback'
$null = [IO.File]::ReadAllText($paths[0].path)
Trace-Stage 'complete'
@{ before = $before; after = $after; readback = $true } | ConvertTo-Json -Depth 6 -Compress
} catch {
    # Never print exception text: a framework error can include paths or data.
    Trace-Stage ('error-hresult:' + $_.Exception.GetBaseException().HResult)
    exit 1
}
`;

// Match the existing native Chat UI fixture's launch environment, including
// Windows profile/temp locations needed by PowerShell and module startup. Do
// not forward provider credentials, application overrides, or PSModulePath.
export function fixturePowerShellEnvironment(inherited) {
  const environment = {};
  const entries = Object.entries(inherited);
  for (const key of ['SystemRoot', 'WINDIR', 'ComSpec', 'TEMP', 'TMP', 'USERPROFILE', 'APPDATA', 'LOCALAPPDATA', 'PATH', 'PATHEXT', 'HOME', 'USER', 'LOGNAME', 'TMPDIR', 'SHELL', 'LANG']) {
    const found = entries.find(([name]) => name.toLowerCase() === key.toLowerCase());
    if (found) { environment[key] = found[1]; }
  }
  if (!environment.SystemRoot || !win32.isAbsolute(environment.SystemRoot)) {
    throw new Error('Windows SystemRoot is required for fixture initialization');
  }
  return environment;
}

function capturedOutput(value, content) {
  let text = String(value ?? '');
  // The fixed script emits only stages and ACL classes. Also redact the exact
  // fixture payload defensively, then bound captured diagnostics on all exits.
  if (content) { text = text.replaceAll(content, '[fixture content redacted]'); }
  return text.replace(/[\u0000-\u0008\u000B\u000C\u000E-\u001F\u007F]/g, '?').slice(0, 8192);
}

function initializationError(completed, content, reason) {
  return new Error(`Windows integration fixture initialization failed: ${reason}; stdout=${capturedOutput(completed.stdout, content)}; stderr=${capturedOutput(completed.stderr, content)}`);
}

function admit(path, directory, previous) {
  const metadata = lstatSync(path);
  if (metadata.isSymbolicLink() || (directory ? !metadata.isDirectory() : !metadata.isFile() || metadata.nlink !== 1)) {
    throw new Error('Invalid owned integration fixture path');
  }
  if (previous && (metadata.dev !== previous.dev || metadata.ino !== previous.ino)) {
    throw new Error('Owned integration fixture identity changed');
  }
  return metadata;
}

/** Create the fixture capability here; no initializer accepts arbitrary paths. */
export function createIntegrationFixture(profileBase, {
  platform = process.platform, environment = process.env, run = spawnSync,
} = {}) {
  const root = mkdtempSync(join(profileBase, 'ma-vs-'));
  const rootIdentity = admit(root, true);
  const configRoot = join(root, 'config');
  mkdirSync(configRoot, { mode: 0o700 });
  const directoryIdentity = admit(configRoot, true);
  const config = join(configRoot, 'config.toml');
  return { root, configRoot, writeConfig(content) {
    admit(root, true, rootIdentity);
    admit(configRoot, true, directoryIdentity);
    writeFileSync(config, content, { mode: 0o600, flag: 'wx' });
    const fileIdentity = admit(config, false);
    let diagnostic;
    if (platform === 'win32') {
      const launchEnvironment = fixturePowerShellEnvironment(environment);
      const completed = run(win32.join(launchEnvironment.SystemRoot, 'System32', 'WindowsPowerShell', 'v1.0', 'powershell.exe'), [
        '-NoLogo', '-NoProfile', '-NonInteractive', '-EncodedCommand',
        Buffer.from(WINDOWS_PRIVATE_FIXTURE, 'utf16le').toString('base64'),
      ], {
        env: { ...launchEnvironment, MINI_AGENT_VSIX_PRIVATE_ROOT: configRoot },
        input: '', encoding: 'utf8', timeout: 30_000, maxBuffer: 64 * 1024, windowsHide: true,
      });
      if (completed.error || completed.status !== 0) {
        throw initializationError(completed, content, completed.error?.code ?? completed.status);
      }
      try {
        diagnostic = JSON.parse(completed.stdout.replace(/^\uFEFF/, ''));
      } catch {
        throw initializationError(completed, content, 'invalid diagnostic JSON');
      }
      if (diagnostic?.readback !== true || diagnostic.after?.length !== 2) {
        throw initializationError(completed, content, 'incomplete initialization');
      }
    }
    admit(root, true, rootIdentity);
    admit(configRoot, true, directoryIdentity);
    admit(config, false, fileIdentity);
    if (readFileSync(config, 'utf8') !== content) { throw new Error('Integration fixture config readback mismatch'); }
    return diagnostic;
  } };
}
