#!/usr/bin/env python3
"""Fail-closed static checks for the Windows MSI release surface."""

from __future__ import annotations

import argparse
import sys
import xml.etree.ElementTree as ET
from pathlib import Path


WIX_NAMESPACE = "http://wixtoolset.org/schemas/v4/wxs"
NS = {"wix": WIX_NAMESPACE}
WIX_VERSION = "6.0.2"


def _require(errors: list[str], condition: bool, message: str) -> None:
    if not condition:
        errors.append(message)


def validate(root: Path) -> list[str]:
    """Validate the checked-in MSI source, helper, and release wiring."""

    errors: list[str] = []
    windows = root / "packaging/windows"
    project_path = windows / "installer.wixproj"
    source_path = windows / "mini-agent.wxs"
    helper_path = windows / "install-vscode-extension.cmd"
    workflow_path = root / ".github/workflows/release.yml"
    ci_path = root / ".github/workflows/ci.yml"

    for path in (project_path, source_path, helper_path, workflow_path, ci_path):
        _require(errors, path.is_file(), f"required MSI file is missing: {path.relative_to(root)}")
    if errors:
        return errors

    try:
        project = ET.parse(project_path).getroot()
    except ET.ParseError as error:
        errors.append(f"installer.wixproj is invalid XML: {error}")
        return errors

    _require(
        errors,
        project.attrib.get("Sdk") == f"WixToolset.Sdk/{WIX_VERSION}",
        f"installer.wixproj must pin WixToolset.Sdk/{WIX_VERSION}",
    )
    extension = project.find(".//PackageReference[@Include='WixToolset.Util.wixext']")
    _require(
        errors,
        extension is not None and extension.attrib.get("Version") == WIX_VERSION,
        f"installer.wixproj must pin WixToolset.Util.wixext {WIX_VERSION}",
    )
    project_text = project_path.read_text(encoding="utf-8")
    for required_input in ("BinaryPath", "VsixPath", "ProductVersion"):
        _require(
            errors,
            f"'$({required_input})' == ''" in project_text,
            f"installer.wixproj must reject a missing {required_input}",
        )

    try:
        source = ET.parse(source_path).getroot()
    except ET.ParseError as error:
        errors.append(f"mini-agent.wxs is invalid XML: {error}")
        return errors

    package = source.find("wix:Package", NS)
    _require(errors, package is not None, "mini-agent.wxs must contain one Package")
    if package is not None:
        _require(
            errors,
            package.attrib.get("Scope") == "perUserOrMachine",
            "MSI must default to a dual-purpose per-user-or-machine package",
        )
        _require(
            errors,
            package.attrib.get("InstallerVersion") == "500",
            "MSI must require Windows Installer 5.0 for dual-purpose scope",
        )

    files = {
        item.attrib.get("Id"): item.attrib
        for item in source.findall(".//wix:File", NS)
    }
    expected_files = {
        "MiniAgentExe": ("$(var.BinaryPath)", "mini-agent.exe"),
        "MiniAgentVsix": ("$(var.VsixPath)", "mini-agent-win32-x64.vsix"),
        "InstallVsCodeExtensionScript": ("install-vscode-extension.cmd", None),
        "LicenseText": ("$(var.RepositoryRoot)\\LICENSE", "LICENSE.txt"),
        "NoticeText": ("$(var.RepositoryRoot)\\NOTICE", "NOTICE.txt"),
        "SourceDirections": ("$(var.RepositoryRoot)\\SOURCE.md", None),
    }
    for file_id, (source_name, installed_name) in expected_files.items():
        attributes = files.get(file_id)
        _require(errors, attributes is not None, f"MSI payload is missing {file_id}")
        if attributes is not None:
            _require(
                errors,
                attributes.get("Source") == source_name,
                f"MSI payload {file_id} has the wrong source",
            )
            if installed_name is not None:
                _require(
                    errors,
                    attributes.get("Name") == installed_name,
                    f"MSI payload {file_id} has the wrong installed name",
                )

    action = source.find(".//wix:CustomAction[@Id='InstallVsCodeExtension']", NS)
    _require(errors, action is not None, "MSI must define the VS Code install custom action")
    if action is not None:
        expected_action = {
            "BinaryRef": "Wix4UtilCA_$(sys.BUILDARCHSHORT)",
            "DllEntry": "WixQuietExec",
            "Execute": "commit",
            "Impersonate": "yes",
            "Return": "ignore",
        }
        for name, value in expected_action.items():
            _require(
                errors,
                action.attrib.get(name) == value,
                f"VS Code custom action must set {name}={value}",
            )

    set_property = source.find(".//wix:SetProperty[@Id='InstallVsCodeExtension']", NS)
    sequence_action = source.find(
        ".//wix:InstallExecuteSequence/wix:Custom[@Action='InstallVsCodeExtension']", NS
    )
    per_user_condition = "NOT Installed AND ALLUSERS <> 1"
    _require(errors, set_property is not None, "MSI must set the VS Code custom-action command")
    if set_property is not None:
        _require(
            errors,
            set_property.attrib.get("Condition") == per_user_condition,
            "VS Code command setup must be restricted to a first per-user install",
        )
    _require(errors, sequence_action is not None, "MSI must schedule the VS Code custom action")
    if sequence_action is not None:
        _require(
            errors,
            sequence_action.attrib.get("Condition") == per_user_condition,
            "VS Code custom action must be restricted to a first per-user install",
        )

    helper = helper_path.read_text(encoding="utf-8")
    for fragment in (
        "where code.cmd",
        "--install-extension \"%MINI_AGENT_VSIX%\" --force",
        "%LOCALAPPDATA%\\Programs\\Microsoft VS Code\\bin\\code.cmd",
        "%ProgramFiles%\\Microsoft VS Code\\bin\\code.cmd",
        "exit /b 0",
    ):
        _require(errors, fragment in helper, f"VS Code install helper is missing {fragment!r}")

    workflow = workflow_path.read_text(encoding="utf-8")
    for fragment in (
        "windows-msi:",
        "python3 scripts/check_windows_msi.py",
        "Get-ChildItem msi-input/mini-agent-*-win32-x64.vsix",
        "/quiet",
        "/norestart",
        "MSI_SHA256SUMS",
        "mini-agent-windows-x64.msi",
    ):
        _require(errors, fragment in workflow, f"release workflow is missing MSI gate {fragment!r}")
    _require(
        errors,
        workflow.count("Start-Process msiexec.exe") >= 2,
        "release workflow must start msiexec.exe for both install and uninstall",
    )
    _require(
        errors,
        workflow.count("-Wait -PassThru") >= 2,
        "release workflow must wait for both msiexec.exe processes and capture their exit codes",
    )

    ci = ci_path.read_text(encoding="utf-8")
    for fragment in (
        "windows-msi:",
        "python scripts/check_windows_msi.py",
        "dotnet build packaging/windows/installer.wixproj",
        "Start-Process msiexec.exe",
        "-Wait -PassThru",
    ):
        _require(errors, fragment in ci, f"CI workflow is missing MSI gate {fragment!r}")
    _require(
        errors,
        ci.count("Start-Process msiexec.exe") >= 2 and ci.count("-Wait -PassThru") >= 2,
        "CI workflow must wait for install and uninstall and capture both exit codes",
    )

    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--root",
        type=Path,
        default=Path(__file__).resolve().parents[1],
        help="repository root",
    )
    args = parser.parse_args()
    errors = validate(args.root.resolve())
    if errors:
        for error in errors:
            print(f"ERROR: {error}", file=sys.stderr)
        return 1
    print("Windows MSI source and release wiring: PASS")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
