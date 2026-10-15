#!/usr/bin/env python3
"""Build a native x64 Windows Git payload in an explicitly provisioned MSYS2 UCRT64 shell.

Run with /usr/bin/python3 (MSYS POSIX Python), not the MinGW/Windows Python.
No toolchain, packages or source are downloaded by this helper.
"""
from __future__ import annotations

import argparse
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import signal
import stat
import struct
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
TARGET = "x86_64-pc-windows-msvc"
# Windows 10+ system contracts only. Every other import must be bundled.
SYSTEM_DLLS = frozenset("""
advapi32.dll authz.dll bcrypt.dll bcryptprimitives.dll cfgmgr32.dll comdlg32.dll
crypt32.dll cryptbase.dll dnsapi.dll gdi32.dll imm32.dll iphlpapi.dll kernel32.dll
msasn1.dll msvcrt.dll ncrypt.dll netapi32.dll normaliz.dll ntdll.dll ole32.dll
oleaut32.dll psapi.dll rpcrt4.dll secur32.dll setupapi.dll shell32.dll shlwapi.dll
ucrtbase.dll user32.dll userenv.dll version.dll winhttp.dll wininet.dll winmm.dll
wintrust.dll wldap32.dll ws2_32.dll
""".split())

# MSYS2 noacl mounts synthesize POSIX permission bits. Establish and inspect the
# native protected DACL instead; never infer cache privacy from chmod/stat.
PRIVATE_CACHE_SCRIPT = r"""
$ErrorActionPreference = 'Stop'
$path = $env:MINI_AGENT_GIT_BUILD_DIRECTORY
$user = [Security.Principal.WindowsIdentity]::GetCurrent().User
$acl = New-Object Security.AccessControl.DirectorySecurity
$acl.SetOwner($user)
$acl.SetAccessRuleProtection($true, $false)
$inherit = [Security.AccessControl.InheritanceFlags]::ContainerInherit -bor `
    [Security.AccessControl.InheritanceFlags]::ObjectInherit
foreach ($sidValue in @($user.Value, 'S-1-5-18', 'S-1-5-32-544')) {
    $sid = New-Object Security.Principal.SecurityIdentifier($sidValue)
    $rule = New-Object Security.AccessControl.FileSystemAccessRule(
        $sid, [Security.AccessControl.FileSystemRights]::FullControl,
        $inherit, [Security.AccessControl.PropagationFlags]::None,
        [Security.AccessControl.AccessControlType]::Allow)
    [void]$acl.AddAccessRule($rule)
}
# This overload applies security at creation, and leaves an existing directory
# unchanged. Existing unsafe caches are rejected by the readback below.
[void][IO.Directory]::CreateDirectory($path, $acl)
if ([IO.File]::GetAttributes($path) -band [IO.FileAttributes]::ReparsePoint) {
    throw 'Git bootstrap cache cannot be a reparse point'
}
$actual = [IO.Directory]::GetAccessControl($path)
$rules = @($actual.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier]) | ForEach-Object {
    @{ sid = $_.IdentityReference.Value; rights = [int]$_.FileSystemRights;
       type = [int]$_.AccessControlType; inherited = $_.IsInherited;
       inheritance = [int]$_.InheritanceFlags; propagation = [int]$_.PropagationFlags }
})
@{ owner = $actual.GetOwner([Security.Principal.SecurityIdentifier]).Value;
   user = $user.Value; protected = $actual.AreAccessRulesProtected; rules = $rules } |
    ConvertTo-Json -Compress -Depth 4
"""


def validate_cache_acl(record: dict) -> None:
    if not isinstance(record, dict) or set(record) != {"owner", "user", "protected", "rules"}:
        raise ValueError("Invalid native Git bootstrap cache security record")
    user = record["user"]
    if not isinstance(user, str) or not re.fullmatch(r"S-1-[0-9-]{1,180}", user):
        raise ValueError("Invalid native Git bootstrap cache owner")
    expected = {user, "S-1-5-18", "S-1-5-32-544"}
    rules = record["rules"]
    if record["owner"] != user or record["protected"] is not True or not isinstance(rules, list) or len(rules) != len(expected):
        raise ValueError("Git bootstrap cache must have an owned protected native DACL")
    seen = set()
    for rule in rules:
        if not isinstance(rule, dict) or set(rule) != {"sid", "rights", "type", "inherited", "inheritance", "propagation"} or rule["sid"] not in expected or rule["sid"] in seen or rule["rights"] != 0x1F01FF or rule["type"] != 0 or rule["inherited"] is not False or rule["inheritance"] != 3 or rule["propagation"] != 0:
            raise ValueError("Git bootstrap cache has an unexpected native access rule")
        seen.add(rule["sid"])


def private_directory(path: Path, env: dict[str, str]) -> None:
    # Reject MSYS symlinks before invoking native APIs; native reparse points
    # are independently rejected after opening the actual Windows path.
    if path.is_symlink() or (path.exists() and not stat.S_ISDIR(path.lstat().st_mode)):
        raise ValueError("Git bootstrap cache must be a real directory")
    system_root = next((value for key, value in env.items() if key.upper() == "SYSTEMROOT"), None)
    if not system_root:
        raise ValueError("Windows SystemRoot is required for native cache security")
    windows_path = subprocess.check_output(["/usr/bin/cygpath", "-aw", str(path)], text=True, timeout=30).strip()
    powershell = subprocess.check_output(["/usr/bin/cygpath", "-u", system_root + "/System32/WindowsPowerShell/v1.0/powershell.exe"], text=True, timeout=30).strip()
    if not windows_path or not powershell:
        raise ValueError("Windows cache security paths are unavailable")
    security_env = env.copy()
    security_env["MINI_AGENT_GIT_BUILD_DIRECTORY"] = windows_path
    response = subprocess.run([powershell, "-NoLogo", "-NoProfile", "-NonInteractive", "-Command", PRIVATE_CACHE_SCRIPT], env=security_env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30, check=True)
    if len(response.stdout) > 16384:
        raise ValueError("Native cache security record exceeds bound")
    validate_cache_acl(json.loads(response.stdout))


def common_module():
    spec = importlib.util.spec_from_file_location("native_git_bootstrap", ROOT / "scripts/build-git-runtime.py")
    if spec is None or spec.loader is None:
        raise ValueError("Native Git bootstrap helpers are unavailable")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def dll_name(value: str) -> str:
    value = value.lower()
    if not re.fullmatch(r"[a-z0-9][a-z0-9_.+-]{0,123}\.dll", value) or ".." in value:
        raise ValueError("Unsafe Windows import DLL name")
    return value


def system_dll(name: str) -> bool:
    return name in SYSTEM_DLLS or bool(re.fullmatch(r"(?:api|ext)-ms-win-[a-z0-9-]+\.dll", name))


def pe_dependencies(data: bytes) -> set[str]:
    """Bounded PE32+ AMD64 import, delay-import and export-forwarder names.

    Layout follows Microsoft's PE/COFF specification. No payload is executed.
    """
    def unpack(fmt, offset):
        if offset < 0 or offset + struct.calcsize(fmt) > len(data):
            raise ValueError("Truncated Windows PE image")
        return struct.unpack_from(fmt, data, offset)

    if data[:2] != b"MZ":
        raise ValueError("Windows Git payload is not a PE image")
    pe, = unpack("<I", 0x3C)
    if data[pe:pe + 4] != b"PE\0\0":
        raise ValueError("Invalid Windows PE signature")
    machine, count, _, _, _, optional_size, _ = unpack("<HHIIIHH", pe + 4)
    optional = pe + 24
    if machine != 0x8664 or not 1 <= count <= 96 or optional_size < 240 or unpack("<H", optional)[0] != 0x20B:
        raise ValueError("Windows Git payload must be native AMD64 PE32+")
    directories, = unpack("<I", optional + 108)
    if directories < 14:
        raise ValueError("Incomplete Windows PE directory table")
    unpack(f"<{optional_size}s", optional)
    sections = []
    for index in range(count):
        header = optional + optional_size + index * 40
        _, virtual_size, rva, raw_size, raw_start = unpack("<8sIIII", header)
        unpack("<40s", header)
        if raw_start + raw_size > len(data):
            raise ValueError("Windows PE section extends beyond file")
        sections.append((rva, virtual_size, raw_start, raw_size))

    def at(rva: int, size: int) -> int:
        matches = [raw + rva - start for start, _, raw, length in sections if start <= rva and rva + size <= start + length]
        if len(matches) != 1:
            raise ValueError("Invalid or ambiguous Windows PE data RVA")
        return matches[0]

    def string(rva: int) -> str:
        offset = at(rva, 1)
        end = data.find(b"\0", offset, min(len(data), offset + 256))
        if end == -1:
            raise ValueError("Unterminated Windows PE dependency name")
        at(rva, end - offset + 1)
        try:
            return data[offset:end].decode("ascii")
        except UnicodeDecodeError as error:
            raise ValueError("Non-ASCII Windows PE dependency name") from error

    dependencies = set()
    for directory, width, name_offset in ((1, 20, 12), (13, 32, 4)):
        table, size = unpack("<II", optional + 112 + directory * 8)
        if not table and not size:
            continue
        if not table or not width <= size <= width * 1024:
            raise ValueError("Invalid Windows PE import table bound")
        terminated = False
        for index in range(size // width):
            entry = at(table + index * width, width)
            if data[entry:entry + width] == bytes(width):
                terminated = True
                break
            if directory == 13 and unpack("<I", entry)[0] != 1:
                raise ValueError("Only RVA-based PE delay imports are supported")
            name, = unpack("<I", entry + name_offset)
            dependencies.add(dll_name(string(name)))
        if not terminated:
            raise ValueError("Unterminated Windows PE import table")
    exports, size = unpack("<II", optional + 112)
    if exports or size:
        if not exports or size < 40:
            raise ValueError("Invalid PE export directory")
        entry = at(exports, 40)
        functions, = unpack("<I", entry + 20)
        addresses, = unpack("<I", entry + 28)
        if functions > 65536:
            raise ValueError("PE export table exceeds bound")
        for index in range(functions):
            address, = unpack("<I", at(addresses + index * 4, 4))
            if exports <= address < exports + size:
                forwarder = string(address)
                if "." not in forwarder:
                    raise ValueError("Invalid PE export forwarder")
                library, _ = forwarder.rsplit(".", 1)
                dependencies.add(dll_name(library if library.lower().endswith(".dll") else library + ".dll"))
    return dependencies


def dependency_closure(programs: list[Path], library_bin: Path, read) -> list[Path]:
    candidates = {}
    for index, file in enumerate(library_bin.iterdir()):
        if index >= 8192:
            raise ValueError("Provisioned Windows library directory exceeds bound")
        if file.suffix.lower() == ".dll":
            name = dll_name(file.name)
            if name in candidates:
                raise ValueError("Case-colliding toolchain DLL names")
            candidates[name] = file
    pending = list(programs)
    captured = {}
    total = 0
    while pending:
        path = pending.pop()
        data = read(path, 64 * 1024 * 1024)
        total += len(data)
        if total > 256 * 1024 * 1024:
            raise ValueError("Windows Git dependency closure exceeds bound")
        for name in pe_dependencies(data):
            if name.startswith(("msys-", "cygwin")):
                raise ValueError("Native Windows Git cannot depend on an MSYS/Cygwin runtime")
            if system_dll(name):
                continue
            if name not in candidates:
                raise ValueError(f"Missing explicitly provisioned Windows DLL: {name}")
            if name not in captured:
                if len(captured) >= 125:
                    raise ValueError("Too many Windows Git dependency DLLs")
                captured[name] = candidates[name]
                pending.append(candidates[name])
    return [captured[name] for name in sorted(captured)]


def toolchain_environment(prefix: Path) -> dict[str, str]:
    if os.name != "posix" or not sys.platform.startswith(("msys", "cygwin")) or os.environ.get("MSYSTEM") != "UCRT64":
        raise ValueError("Run with MSYS2 /usr/bin/python3 in a native UCRT64 shell")
    if not prefix.is_absolute() or not re.fullmatch(r"/[A-Za-z0-9_./-]+", str(prefix)) or ".." in prefix.parts:
        raise ValueError("UCRT64 prefix must be an explicit absolute build path without Make metacharacters")
    env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_") and key not in {"MINI_AGENT_GIT_GLOBAL_SNAPSHOT", "MAKEFLAGS", "MFLAGS"}}
    env.update(PATH=str(prefix / "bin") + ":/usr/bin:/bin", MINGW_PREFIX=str(prefix), MSYSTEM="UCRT64")
    machine = subprocess.check_output([str(prefix / "bin/gcc"), "-dumpmachine"], env=env, text=True, timeout=30).strip()
    if machine != "x86_64-w64-mingw32":
        raise ValueError("UCRT64 compiler is not native x86-64 MinGW")
    backends = subprocess.check_output([str(prefix / "bin/curl-config"), "--ssl-backends"], env=env, text=True, timeout=30).strip().lower()
    if backends != "schannel":
        raise ValueError("Provision curl-winssl with Schannel only; no external CA/provider path is bundled")
    return env


def require_cmp(env: dict[str, str]) -> None:
    # Git 2.56's FORCE-updated, included GIT-VERSION-FILE uses cmp to avoid
    # rewriting unchanged bytes. Without it, make restarts indefinitely.
    try:
        subprocess.run(["/usr/bin/cmp", "-s", "/dev/null", "/dev/null"], env=env,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                       timeout=10, check=True)
    except (OSError, subprocess.SubprocessError) as error:
        raise ValueError("Required /usr/bin/cmp is missing or unusable; install MSYS2 diffutils before building Git") from error


def require_pcre2_header(prefix: Path, env: dict[str, str]) -> None:
    # The pinned MinGW configuration enables PCRE2. Check its actual header
    # with the already-validated compiler, without writing or running a binary.
    source = b"#define PCRE2_CODE_UNIT_WIDTH 8\n#include <pcre2.h>\n"
    try:
        subprocess.run([str(prefix / "bin/gcc"), "-x", "c", "-fsyntax-only", "-"],
                       input=source, env=env, stdout=subprocess.DEVNULL,
                       stderr=subprocess.DEVNULL, timeout=10, check=True)
    except (OSError, subprocess.SubprocessError) as error:
        raise ValueError("Required UCRT64 pcre2.h is missing or unusable; install mingw-w64-ucrt-x86_64-pcre2 before building Git") from error


def bootstrap(*, output: Path, source_archive: Path, policy_patch: Path, cache_root: Path, toolchain_prefix: Path, jobs: int) -> Path:
    if not 1 <= jobs <= 64:
        raise ValueError("Git bootstrap jobs must be between 1 and 64")
    common = common_module()
    archive = common.read_regular(source_archive, 32 * 1024 * 1024)
    patch = common.read_regular(policy_patch, 1024 * 1024)
    if common.digest(archive) != common.SOURCE_SHA256 or common.digest(patch) != common.PATCH_SHA256:
        raise ValueError("Windows Git source or policy patch does not match pinned hash")
    if common.digest(common.read_regular(ROOT / "scripts/git-runtime/policy.patch", 1024 * 1024)) != common.PATCH_SHA256:
        raise ValueError("Canonical Windows Git policy patch hash mismatch")
    output = output.absolute()
    if output.exists() or output.is_symlink():
        raise ValueError("Windows Git bundle output must not already exist")
    env = toolchain_environment(toolchain_prefix)
    require_cmp(env)
    require_pcre2_header(toolchain_prefix, env)
    cache_root = cache_root.absolute()
    cache_root.parent.mkdir(parents=True, exist_ok=True)
    private_directory(cache_root, env)
    native_cache = cache_root / TARGET
    private_directory(native_cache, env)
    # Fresh build only: CI provisions the compiler and DLLs explicitly. No cache
    # can retain an obsolete dependency package across a toolchain update.
    # Every build owns fresh staging; no shared cached artifact is consumed,
    # so a POSIX flock/mode-based lock is unnecessary on MSYS2 noacl mounts.
    staging = Path(tempfile.mkdtemp(prefix=".windows-building-", dir=native_cache))
    try:
        source = common.extract_source(archive, staging)
        patch_file = staging / "policy.patch"
        patch_file.write_bytes(patch)
        common.run(["/usr/bin/patch", "--batch", "--fuzz=0", "-p1", "-i", str(patch_file)], source, env)
        common.run(["/usr/bin/make", f"-j{jobs}", "NO_RUST=YesPlease", "NO_GETTEXT=YesPlease", "NO_TCLTK=YesPlease", "NO_PERL=YesPlease", "NO_PYTHON=YesPlease", "NO_OPENSSL=YesPlease", "NO_INSTALL_HARDLINKS=YesPlease", "NO_GITWEB=YesPlease", f"CC={toolchain_prefix}/bin/gcc", f"CURL_CONFIG={toolchain_prefix}/bin/curl-config", "CFLAGS=-O2", f"prefix={toolchain_prefix}", "git.exe", "git-remote-http.exe"], source, env)
        programs = [source / "git.exe", source / "git-remote-http.exe"]
        libraries = dependency_closure(programs, toolchain_prefix / "bin", common.read_regular)
        producer = common.prepare_module()
        producer.prepare(root=ROOT, target=TARGET, source_archive=source_archive, git=programs[0], http_helper=programs[1], https_helper=None, libraries=libraries, output=output)
        # Version check uses ONLY the captured payload DLLs and Windows OS
        # directories. UCRT64/bin is deliberately absent from this PATH.
        try:
            runtime_env = env.copy()
            system_root = next((value for key, value in env.items() if key.upper() == "SYSTEMROOT"), None)
            if not system_root:
                raise ValueError("Windows SystemRoot is required for explicit system DLL resolution")
            windows = Path(subprocess.check_output(["/usr/bin/cygpath", "-u", system_root], text=True, timeout=30).strip())
            runtime_env["PATH"] = str(output) + ":" + str(windows / "System32") + ":" + str(windows)
            common.run([str(output / "git.exe"), "--version"], output, runtime_env)
        except BaseException:
            shutil.rmtree(output)
            raise
    finally:
        shutil.rmtree(staging)
    return output


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", default=TARGET, choices=[TARGET])
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--source-archive", type=Path, default=ROOT / "scripts/git-runtime/git-2.56.0.tar.xz")
    parser.add_argument("--policy-patch", type=Path, default=ROOT / "scripts/git-runtime/policy.patch")
    parser.add_argument("--cache-root", type=Path, default=ROOT / "target/git-runtime-bootstrap")
    parser.add_argument("--toolchain-prefix", type=Path, default=Path("/ucrt64"))
    parser.add_argument("--jobs", type=int, default=min(os.cpu_count() or 1, 8))
    args = vars(parser.parse_args())
    args.pop("target")
    def interrupt(_signum, _frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, interrupt)
    try:
        print(bootstrap(**args))
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        parser.exit(1, f"Windows Git runtime bootstrap failed: {error}\n")


if __name__ == "__main__":
    main()
