# Windows MSI

The Windows installer is a WiX 6.0.2 dual-purpose MSI. It installs per-user by default to
`%LOCALAPPDATA%\Programs\mini-agent` without elevation. Administrators can request a per-machine
install under `%ProgramFiles%\mini-agent` with standard Windows Installer properties.

Per-machine launches rely on the normal inherited Windows `ALL APPLICATION PACKAGES`
read/execute grant. mini-agent verifies that effective access and never attempts to rewrite the
protected executable's ACL; a hardened enterprise ACL that removes this access causes the JS and
shell containment preflights to fail closed with an explicit startup error.

The package contains `mini-agent.exe`, the native win32-x64 VSIX, the release's GPL notice and
Corresponding Source directions, and `THIRD_PARTY_LICENSES.txt`, the Rust dependency license
inventory taken from the same release archive as the executable. After a successful first install, a commit custom action looks
for `code.cmd` on `PATH` and in the standard user and machine VS Code locations. If VS Code is
present it installs the bundled VSIX with `--force`; absence or extension-install failure does not
roll back the binary installation. A service-account GPO deployment therefore installs the MSI
payload but does not mutate another user's VS Code profile.

Build from a Windows checkout with the .NET 6 or newer SDK:

```powershell
dotnet build packaging/windows/installer.wixproj `
  --configuration Release `
  --output msi-output `
  -p:ProductVersion=1.9.4 `
  -p:BinaryPath=C:\artifacts\mini-agent.exe `
  -p:VsixPath=C:\artifacts\mini-agent-1.9.4-win32-x64.vsix `
  -p:ThirdPartyLicensesPath=C:\artifacts\THIRD_PARTY_LICENSES
```

Silent per-user install (the default):

```powershell
msiexec /i mini-agent-windows-x64.msi /quiet /norestart
```

Silent per-machine install for GPO, Intune, or SCCM:

```powershell
msiexec /i mini-agent-windows-x64.msi ALLUSERS=1 /quiet /norestart
```

## Code signing status

The MSI and the `mini-agent.exe` it installs are **not Authenticode-signed**: the project has no
code-signing certificate yet, and the release workflow runs no `signtool` step. Consequences:

- Opening a downloaded MSI shows the SmartScreen "Windows protected your PC" dialog. Choose
  **More info**, then **Run anyway**, only after verifying the file as described below.
- AppLocker and WDAC (App Control for Business) publisher rules cannot pin a mini-agent signer
  identity. Allow the package with file-hash rules for the verified MSI and `mini-agent.exe`
  instead, and update those rules on every release. A default-deny publisher-only policy blocks the
  installer and the binary.
- `Get-AuthenticodeSignature` reports `NotSigned` for both files; that is expected until signing
  exists.

Verify the download before installing, silently or interactively:

```powershell
# SHA-256 must match the line in MSI_SHA256SUMS from the same release
Get-FileHash mini-agent-windows-x64.msi -Algorithm SHA256
Get-Content MSI_SHA256SUMS

# SLSA build provenance produced by the release workflow
gh attestation verify mini-agent-windows-x64.msi --repo sebahrens/mini-agent
```

## Release smoke

The release workflow performs a real quiet per-user install, binary smoke, uninstall, and MSI
checksum generation on `windows-latest` before the artifact can be published.
