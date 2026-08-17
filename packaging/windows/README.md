# Windows MSI

The Windows installer is a WiX 6.0.2 dual-purpose MSI. It installs per-user by default to
`%LOCALAPPDATA%\Programs\mini-agent` without elevation. Administrators can request a per-machine
install under `%ProgramFiles%\mini-agent` with standard Windows Installer properties.

The package contains `mini-agent.exe`, the native win32-x64 VSIX, and the release's GPL notice and
Corresponding Source directions. After a successful first install, a commit custom action looks
for `code.cmd` on `PATH` and in the standard user and machine VS Code locations. If VS Code is
present it installs the bundled VSIX with `--force`; absence or extension-install failure does not
roll back the binary installation. A service-account GPO deployment therefore installs the MSI
payload but does not mutate another user's VS Code profile.

Build from a Windows checkout with the .NET 6 or newer SDK:

```powershell
dotnet build packaging/windows/installer.wixproj `
  --configuration Release `
  --output msi-output `
  -p:ProductVersion=1.8.0 `
  -p:BinaryPath=C:\artifacts\mini-agent.exe `
  -p:VsixPath=C:\artifacts\mini-agent-1.8.0-win32-x64.vsix
```

Silent per-user install (the default):

```powershell
msiexec /i mini-agent-windows-x64.msi /quiet /norestart
```

Silent per-machine install for GPO, Intune, or SCCM:

```powershell
msiexec /i mini-agent-windows-x64.msi ALLUSERS=1 /quiet /norestart
```

The release workflow performs a real quiet per-user install, binary smoke, uninstall, and MSI
checksum generation on `windows-latest` before the artifact can be published.
