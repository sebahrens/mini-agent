# Install mini-agent on Windows

The supported x86-64 Windows packages are published on the
[GitHub Releases page](https://github.com/sebahrens/mini-agent/releases/latest). The VS Code extension
is not currently published in the Visual Studio Marketplace, so search results there are not an
installation source for mini-agent.

## Recommended: install the MSI

1. Open the latest GitHub release in a browser.
2. Download `mini-agent-windows-x64.msi` and `MSI_SHA256SUMS` into the same folder.
3. In PowerShell, verify the downloaded file before opening it:

   ```powershell
   $asset = 'mini-agent-windows-x64.msi'
   $line = Get-Content .\MSI_SHA256SUMS | Where-Object { $_ -match "\s+$([regex]::Escape($asset))$" }
   if (-not $line) { throw "No checksum published for $asset" }
   $expected = ($line -split '\s+')[0].ToLowerInvariant()
   $actual = (Get-FileHash ".\$asset" -Algorithm SHA256).Hash.ToLowerInvariant()
   if ($actual -ne $expected) { throw "SHA-256 mismatch for $asset" }
   "Verified $asset ($actual)"
   ```

4. Open the verified MSI. The default installation is per-user, requires no administrator rights,
   installs `mini-agent.exe`, and attempts to install its bundled VSIX when VS Code is present.
5. Open a new PowerShell window and run `mini-agent --version` and `mini-agent --js-runtime-check`.

For managed deployment, an administrator can use:

```powershell
msiexec /i .\mini-agent-windows-x64.msi ALLUSERS=1 /quiet /norestart
```

A service-account install does not modify another user's VS Code profile. Install the VSIX for each
intended VS Code user as described below.

## Install only the VS Code extension

Download the `mini-agent-<version>-win32-x64.vsix` asset and `VSIX_SHA256SUMS` from the same release.
Verify it with the same procedure, changing `$asset` and the checksum filename, then install it:

```powershell
$asset = (Get-ChildItem .\mini-agent-*-win32-x64.vsix -File | Select-Object -First 1).Name
$line = Get-Content .\VSIX_SHA256SUMS | Where-Object { $_ -match "\s+$([regex]::Escape($asset))$" }
if (-not $line) { throw "No checksum published for $asset" }
$expected = ($line -split '\s+')[0].ToLowerInvariant()
$actual = (Get-FileHash ".\$asset" -Algorithm SHA256).Hash.ToLowerInvariant()
if ($actual -ne $expected) { throw "SHA-256 mismatch for $asset" }
code --install-extension ".\$asset" --force
```

The extension needs the native `mini-agent.exe`; installing the MSI is the simplest way to provide
both. If policy prevents the MSI from installing the extension automatically, the command above is
the supported manual path.

## Enterprise download policy

If direct PowerShell or command-line downloads fail because of corporate TLS, proxy, or revocation
policy, use the organization's managed browser, software portal, or an IT-approved internal mirror.
Transfer both the artifact and its checksum file, then perform the same local SHA-256 comparison.
Do not disable certificate validation, revocation checks, or endpoint protection, and do not use
`curl -k`, `Invoke-WebRequest` certificate-bypass callbacks, or similar workarounds.

If an internal mirror renames an asset, obtain the original release filename and digest from the
release record and compare the digest explicitly. A successful checksum verifies the transferred
bytes; it does not make an untrusted download location authoritative.

## Clean-profile verification

For installation testing, use a newly created standard Windows user with no existing mini-agent
configuration. Verify that the MSI installs without elevation, a new PowerShell resolves
`mini-agent.exe`, `mini-agent --version` succeeds, and `mini-agent --js-runtime-check` reports `PASS`.
If VS Code is installed for that user, confirm the mini-agent extension appears in the Extensions
view. Uninstall from **Installed apps** and confirm a new PowerShell no longer resolves
`mini-agent.exe`.
