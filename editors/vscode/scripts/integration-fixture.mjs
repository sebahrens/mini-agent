import { spawnSync } from 'node:child_process';
import { lstatSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { join, win32 } from 'node:path';

// This script only receives paths created by createIntegrationFixture below.
// Protect the leaf first: tightening a parent can remove an unprotected child's
// inherited grants before the child's own ACL can be initialized.
const WINDOWS_PRIVATE_FIXTURE = String.raw`
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = New-Object Text.UTF8Encoding($false)
$user = [Security.Principal.WindowsIdentity]::GetCurrent().User
$system = New-Object Security.Principal.SecurityIdentifier('S-1-5-18')
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
$paths = @(
    @{ path = (Join-Path $env:MINI_AGENT_VSIX_PRIVATE_ROOT 'config.toml'); relative = 'config.toml'; directory = $false },
    @{ path = $env:MINI_AGENT_VSIX_PRIVATE_ROOT; relative = '.'; directory = $true }
)
$before = @()
foreach ($entry in $paths) {
    $item = Get-Item -LiteralPath $entry.path -Force
    if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -or $item.PSIsContainer -ne $entry.directory) { throw 'Invalid owned fixture path' }
    $before += Inspect-Acl $entry.path $entry.relative
}
foreach ($entry in $paths) {
    $acl = if ($entry.directory) { New-Object Security.AccessControl.DirectorySecurity } else { New-Object Security.AccessControl.FileSecurity }
    $acl.SetOwner($user)
    $acl.SetAccessRuleProtection($true, $false)
    foreach ($sid in @($user, $system)) {
        $rule = New-Object Security.AccessControl.FileSystemAccessRule($sid, [Security.AccessControl.FileSystemRights]::FullControl, [Security.AccessControl.InheritanceFlags]::None, [Security.AccessControl.PropagationFlags]::None, [Security.AccessControl.AccessControlType]::Allow)
        [void]$acl.AddAccessRule($rule)
    }
    Set-Acl -LiteralPath $entry.path -AclObject $acl
}
$after = @()
foreach ($entry in $paths) {
    $row = Inspect-Acl $entry.path $entry.relative
    if (-not $row.ownerCurrentUser -or -not $row.protected -or $row.rules.Count -ne 2) { throw 'Private fixture owner/DACL mismatch' }
    foreach ($rule in $row.rules) {
        if ($rule.trustee -notin @('current-user', 'SYSTEM') -or $rule.inherited -or $rule.inheritance -ne 0 -or $rule.propagation -ne 0 -or -not $rule.allow -or -not $rule.fullControl) { throw 'Private fixture ACL mismatch' }
    }
    if (@($row.rules | Where-Object { $_.trustee -eq 'current-user' }).Count -ne 1) { throw 'Private fixture trustees mismatch' }
    $after += $row
}
# Read after parent tightening, without emitting config contents or credentials.
$null = [IO.File]::ReadAllText($paths[0].path)
@{ before = $before; after = $after; readback = $true } | ConvertTo-Json -Depth 6 -Compress
`;

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
      const systemRoot = environment.SystemRoot || environment.SYSTEMROOT;
      if (!systemRoot || !win32.isAbsolute(systemRoot)) { throw new Error('Windows SystemRoot is required for fixture initialization'); }
      const completed = run(win32.join(systemRoot, 'System32', 'WindowsPowerShell', 'v1.0', 'powershell.exe'), [
        '-NoLogo', '-NoProfile', '-NonInteractive', '-EncodedCommand',
        Buffer.from(WINDOWS_PRIVATE_FIXTURE, 'utf16le').toString('base64'),
      ], {
        env: { SystemRoot: systemRoot, MINI_AGENT_VSIX_PRIVATE_ROOT: configRoot },
        input: '', encoding: 'utf8', timeout: 30_000, maxBuffer: 64 * 1024, windowsHide: true,
      });
      if (completed.error || completed.status !== 0) {
        throw new Error(`Windows integration fixture initialization failed: ${completed.error?.message ?? completed.stderr}`);
      }
      diagnostic = JSON.parse(completed.stdout.replace(/^\uFEFF/, ''));
      if (diagnostic.readback !== true || diagnostic.after?.length !== 2) {
        throw new Error('Incomplete Windows integration fixture initialization');
      }
    }
    admit(root, true, rootIdentity);
    admit(configRoot, true, directoryIdentity);
    admit(config, false, fileIdentity);
    if (readFileSync(config, 'utf8') !== content) { throw new Error('Integration fixture config readback mismatch'); }
    return diagnostic;
  } };
}
