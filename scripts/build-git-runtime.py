#!/usr/bin/env python3
"""Build the pinned, policy-patched native Git runtime without running Cargo."""
from __future__ import annotations

import argparse
import contextlib
import hashlib
import importlib.util
import io
import os
from pathlib import Path, PurePosixPath
import platform
import shutil
import shlex
import signal
import stat
import struct
import subprocess
import sys
import tarfile
import tempfile

SOURCE_SHA256 = "26c56c296b38c0695b26fa95f475f1d01704d2d38e73465ca30b0b2f5dc789d3"
PATCH_SHA256 = "3138c02584ea00a7234eef01366b0ca5757c0b5e4f28b4878381544b0b0e97b2"
POLICY = "mini-agent-executable-scope-v3-env"
TARGETS = (
    "x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin", "aarch64-apple-darwin", "x86_64-pc-windows-msvc",
)
ROOT = Path(__file__).resolve().parents[1]


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def read_regular(path: Path, limit: int) -> bytes:
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_size > limit:
            raise ValueError("Git bootstrap input is not a bounded regular file")
        data = stream.read(limit + 1)
    if len(data) > limit:
        raise ValueError("Git bootstrap input exceeds its bound")
    return data


def native_target() -> str:
    machine = {"arm64": "aarch64", "aarch64": "aarch64", "x86_64": "x86_64", "AMD64": "x86_64"}.get(platform.machine())
    if machine is None:
        raise ValueError("Unsupported native Git bootstrap architecture")
    if sys.platform == "darwin":
        return machine + "-apple-darwin"
    if sys.platform.startswith("linux"):
        try:
            gnu = os.confstr("CS_GNU_LIBC_VERSION")
        except (ValueError, OSError):
            gnu = None
        if gnu and gnu.startswith("glibc "):
            return machine + "-unknown-linux-gnu"
        probe = subprocess.run(["ldd", "--version"], capture_output=True, text=True, timeout=10)
        if "musl libc" in probe.stdout + probe.stderr:
            return machine + "-unknown-linux-musl"
    raise ValueError("Native bootstrap requires macOS, GNU/Linux or musl Linux")


def require_target(target: str) -> None:
    if target not in TARGETS:
        raise ValueError("Unsupported Git runtime target")
    if "msvc" in target:
        raise ValueError("Windows requires the explicit UCRT64 build helper")
    if target != native_target():
        raise ValueError("Git bootstrap refuses cross-compilation or a mislabeled native payload")


def extract_source(data: bytes, destination: Path) -> Path:
    """Extract only bounded, unique members and internal links into owned staging."""
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:xz") as archive:
        members = archive.getmembers()
        if len(members) > 10000 or sum(m.size for m in members) > 128 * 1024 * 1024:
            raise ValueError("Git source archive exceeds extraction bounds")
        seen: set[str] = set()
        links = []
        link_paths = {PurePosixPath(m.name) for m in members if m.issym()}
        for member in members:
            path = PurePosixPath(member.name)
            normalized = str(path)
            if path.is_absolute() or ".." in path.parts or not path.parts or path.parts[0] != "git-2.56.0" or normalized in seen or any(parent in link_paths for parent in path.parents):
                raise ValueError("Unsafe or duplicate Git source archive path")
            seen.add(normalized)
            output = destination.joinpath(*path.parts)
            if member.isdir():
                output.mkdir(mode=0o700, parents=True, exist_ok=True)
            elif member.isfile():
                output.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
                source = archive.extractfile(member)
                if source is None:
                    raise ValueError("Missing Git source member")
                with source, output.open("xb") as writer:
                    shutil.copyfileobj(source, writer)
                output.chmod(0o700 if member.mode & 0o111 else 0o600)
            elif member.issym():
                target = PurePosixPath(member.linkname)
                # Resolve lexically without following any archive-created links.
                parts = list(path.parent.parts)
                if target.is_absolute():
                    raise ValueError("Absolute Git source link")
                for part in target.parts:
                    if part == "..":
                        if len(parts) <= 1:
                            raise ValueError("Escaping Git source link")
                        parts.pop()
                    elif part != ".":
                        parts.append(part)
                links.append((output, member.linkname))
            else:
                raise ValueError("Unsupported Git source archive member")
        # No regular-file extraction can traverse a previously created symlink.
        for output, target in links:
            output.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
            output.symlink_to(target)
    return destination / "git-2.56.0"


def verify_binary(path: Path, target: str) -> None:
    data = read_regular(path, 64 * 1024 * 1024)
    arch = target.split("-", 1)[0]
    if "apple" in target:
        expected = 0x0100000C if arch == "aarch64" else 0x01000007
        okay = len(data) >= 8 and data[:4] == b"\xcf\xfa\xed\xfe" and struct.unpack_from("<I", data, 4)[0] == expected
    else:
        expected = 183 if arch == "aarch64" else 62
        okay = len(data) >= 20 and data[:6] == b"\x7fELF\x02\x01" and struct.unpack_from("<H", data, 18)[0] == expected
    if not okay:
        raise ValueError("Built Git executable does not match the native target")
    if "musl" in target:
        if len(data) < 64:
            raise ValueError("Truncated static Git ELF header")
        phoff = struct.unpack_from("<Q", data, 32)[0]
        phsize, phcount = struct.unpack_from("<HH", data, 54)
        if phsize < 56 or not 0 < phcount <= 1024 or phoff + phsize * phcount > len(data):
            raise ValueError("Invalid static Git ELF program headers")
        for index in range(phcount):
            kind = struct.unpack_from("<I", data, phoff + index * phsize)[0]
            if kind == 3:
                raise ValueError("Musl Git payload must not require a dynamic loader")
            if kind == 2:
                # Static PIE contains self-relocation metadata but no loader or
                # external libraries. Check the dynamic entries, not its label.
                offset = struct.unpack_from("<Q", data, phoff + index * phsize + 8)[0]
                size = struct.unpack_from("<Q", data, phoff + index * phsize + 32)[0]
                if not 16 <= size <= 65536 or size % 16 or offset + size > len(data):
                    raise ValueError("Invalid static Git dynamic metadata")
                terminated = False
                for entry in range(offset, offset + size, 16):
                    tag = struct.unpack_from("<q", data, entry)[0]
                    if tag in (1, 15, 29, 0x7FFFFFFD, 0x7FFFFFFF):
                        raise ValueError("Musl Git payload contains an external dynamic dependency")
                    if tag == 0:
                        terminated = True
                        break
                if not terminated:
                    raise ValueError("Unterminated static Git dynamic metadata")


def namespace_static_error(source: Path) -> None:
    # GNU libidn2's static gnulib dependency exports an unrelated error(3).
    # Keep its ABI separate from Git's variadic error() rather than accepting
    # duplicate symbols or depending on which archive the linker sees first.
    replacements = {
        "git-compat-util.h": [
            ("int error(const char *err, ...)", "int mini_agent_git_error(const char *err, ...)"),
            ("#define error(...) (error(__VA_ARGS__), const_error())", "#define error(...) (mini_agent_git_error(__VA_ARGS__), const_error())"),
        ],
        "usage.c": [("int error(const char *err, ...)", "int mini_agent_git_error(const char *err, ...)")],
    }
    for name, entries in replacements.items():
        path = source / name
        text = path.read_text()
        for old, new in entries:
            if text.count(old) != 1:
                raise ValueError("Pinned Git static error namespace context changed")
            text = text.replace(old, new)
        path.write_text(text)


def make_arguments(target: str, jobs: int, env: dict[str, str]) -> list[str]:
    arguments = ["make", f"-j{jobs}", "NO_RUST=YesPlease", "NO_GETTEXT=YesPlease", "NO_TCLTK=YesPlease", "NO_PERL=YesPlease", "NO_PYTHON=YesPlease", "NO_INSTALL_HARDLINKS=YesPlease", "NO_GITWEB=YesPlease", "NO_SUBMODULE_LINT=YesPlease", "CC=cc", "CFLAGS=-O2", "prefix=/usr"]
    if "musl" in target:
        # pkg-config's complete static closure includes the provisioned curl TLS,
        # compression and resolver libraries; the resulting ELF is checked below.
        backend = subprocess.check_output(["curl-config", "--ssl-backends"], env=env, text=True, timeout=30).strip()
        if backend.casefold() != "mbedtls":
            raise ValueError("Static musl Git requires the documented Mbed TLS curl toolchain")
        curl = subprocess.check_output(["pkg-config", "--static", "--libs", "libcurl"], env=env, text=True, timeout=30).strip()
        if not curl or "\n" in curl or "\r" in curl:
            raise ValueError("Invalid static libcurl link closure")
        arguments += ["LDFLAGS=-static", "NO_REGEX=NeedsStartEnd", "NO_EXPAT=YesPlease", "NO_OPENSSL=YesPlease", f"CURL_LDFLAGS={curl}"]
    return arguments + ["git", "git-remote-http"]


def run(command: list[str], cwd: Path, env: dict[str, str]) -> None:
    # Own the complete make process group through interruption before removing
    # its staging directory. Keep HOME and compiler diagnostics unchanged.
    child = subprocess.Popen(command, cwd=cwd, env=env, stdout=sys.stderr, stderr=sys.stderr, start_new_session=True)
    try:
        status = child.wait(timeout=900)
    except BaseException:
        try:
            os.killpg(child.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        # Reap only after the group is killed, so its leader PID cannot be
        # recycled between wait() and the final group signal.
        try:
            os.killpg(child.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        child.wait()
        raise
    if status:
        raise subprocess.CalledProcessError(status, command)


def prepare_module():
    spec = importlib.util.spec_from_file_location("prepare_git_runtime", ROOT / "scripts/prepare-git-runtime.py")
    if spec is None or spec.loader is None:
        raise ValueError("Git bundle producer is unavailable")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def validate_bundle(bundle: Path, target: str) -> None:
    if bundle.is_symlink() or not bundle.is_dir():
        raise ValueError("Invalid Git bootstrap cache bundle")
    lines = read_regular(bundle / "bundle.manifest", 64 * 1024).decode("utf-8").splitlines()
    expected = ["mini-agent-git-runtime-v1", f"target={target}", f"source-sha256={SOURCE_SHA256}", f"policy-sha256={PATCH_SHA256}", f"policy={POLICY}"]
    if lines[:5] != expected or len(lines) != 8:
        raise ValueError("Git bootstrap cache manifest does not match pinned inputs")
    names = set()
    for line in lines[5:]:
        fields = line.split("\t")
        if len(fields) != 3 or not fields[0].startswith("file=") or fields[2] != "x":
            raise ValueError("Invalid cached Git executable record")
        name = fields[0][5:]
        if name not in {"git", "git-remote-http", "git-remote-https"} or name in names:
            raise ValueError("Invalid cached Git executable name")
        names.add(name)
        if digest(read_regular(bundle / name, 64 * 1024 * 1024)) != fields[1]:
            raise ValueError("Cached Git executable hash mismatch")
        verify_binary(bundle / name, target)
    if {p.name for p in bundle.iterdir()} != names | {"bundle.manifest"}:
        raise ValueError("Unexpected file in cached Git bundle")


PACKAGE_HINT = {
    "cc": "Install a C toolchain: Debian/Ubuntu `apt install build-essential`, Fedora/RHEL `dnf install gcc make`, macOS `xcode-select --install`.",
    "curl-config": "Install the curl development package: Debian/Ubuntu `apt install libcurl4-openssl-dev`, Fedora/RHEL `dnf install libcurl-devel`, macOS ships curl-config with the Xcode command line tools.",
    "pkg-config": "Install pkg-config: Debian/Ubuntu `apt install pkg-config`, Fedora/RHEL `dnf install pkgconf-pkg-config`.",
}
PACKAGE_HINT_TAIL = " Full list: build-essential libcurl4-openssl-dev zlib1g-dev python3 (see docs/development/git-runtime-bootstrap.md), or set MINI_AGENT_GIT_RUNTIME_BUNDLE."


def toolchain_identity(env: dict[str, str], target: str, inputs: list[Path] | None = None) -> str:
    # A source-only cache key could silently reuse an OpenSSL build after the
    # publisher selected Mbed TLS. Bind the actual compiler and curl backend.
    identity = hashlib.sha256()
    for variable in ("SDKROOT", "MACOSX_DEPLOYMENT_TARGET", "CPPFLAGS", "LDFLAGS", "PKG_CONFIG_PATH"):
        value = env.get(variable, "").encode(errors="surrogateescape")
        identity.update(len(value).to_bytes(8, "little"))
        identity.update(value)
    for program in ["cc", "curl-config"] + (["pkg-config"] if "musl" in target else []):
        executable = shutil.which(program, path=env.get("PATH"))
        if executable is None:
            raise ValueError(f"Git build toolchain program is unavailable: {program}. {PACKAGE_HINT.get(program, '')}{PACKAGE_HINT_TAIL}")
        path = Path(executable).absolute()
        resolved = path.resolve(strict=True)
        identity.update(hashlib.sha256(read_regular(resolved, 256 * 1024 * 1024)).digest())
        if inputs is not None:
            inputs.extend([path, resolved])
    for command in (["cc", "--version"], ["cc", "-dumpmachine"], ["curl-config", "--version"], ["curl-config", "--ssl-backends"], ["curl-config", "--static-libs"]):
        output = subprocess.check_output(command, env=env, timeout=30)
        if isinstance(output, str):
            output = output.encode()
        if len(output) > 65536:
            raise ValueError("Git build toolchain identity exceeds its bound")
        identity.update(len(output).to_bytes(8, "little"))
        identity.update(output)
    if "musl" in target:
        flags = shlex.split(subprocess.check_output(["pkg-config", "--static", "--libs", "libcurl"], env=env, text=True, timeout=30))
        directories = [Path(flag[2:]) for flag in flags if flag.startswith("-L")]
        libraries = list(dict.fromkeys([flag[2:] for flag in flags if flag.startswith("-l")] + ["c", "gcc", "gcc_eh"]))
        if len(libraries) > 64:
            raise ValueError("Static Git library closure exceeds its bound")
        for library in libraries:
            if not library or not all(byte.isalnum() or byte in "_-+" for byte in library):
                raise ValueError("Static Git library name is invalid")
            filename = "lib" + library + ".a"
            path = next((directory / filename for directory in directories if (directory / filename).is_file()), None)
            if path is None:
                path = Path(subprocess.check_output(["cc", "--print-file-name=" + filename], env=env, text=True, timeout=30).strip())
            if not path.is_absolute():
                raise ValueError("Static Git library cannot be resolved")
            path = path.resolve(strict=True)
            contents = read_regular(path, 64 * 1024 * 1024)
            if inputs is not None:
                inputs.append(path)
            identity.update(library.encode())
            identity.update(hashlib.sha256(contents).digest())
    return identity.hexdigest()


def private_directory(path: Path) -> None:
    try:
        path.mkdir(mode=0o700)
    except FileExistsError:
        pass
    info = path.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise ValueError("Git bootstrap cache must be an owned private directory")


@contextlib.contextmanager
def cache_lock(path: Path):
    import fcntl
    fd = os.open(path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK, 0o600)
    try:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_nlink != 1 or info.st_mode & 0o077:
            raise ValueError("Invalid Git bootstrap cache lock")
        fcntl.flock(fd, fcntl.LOCK_EX)
        yield
    finally:
        os.close(fd)


def build_environment() -> dict[str, str]:
    env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_") and key != "MINI_AGENT_GIT_GLOBAL_SNAPSHOT"}
    for variable in ("MAKEFLAGS", "MFLAGS", "PROFILE"):
        env.pop(variable, None)
    return env


def bootstrap(*, target: str, output: Path, source_archive: Path, policy_patch: Path, cache_root: Path, jobs: int) -> Path:
    require_target(target)
    if not 1 <= jobs <= 64:
        raise ValueError("Git bootstrap jobs must be between 1 and 64")
    output, source_archive, policy_patch, cache_root = (p.absolute() for p in (output, source_archive, policy_patch, cache_root))
    archive = read_regular(source_archive, 32 * 1024 * 1024)
    patch = read_regular(policy_patch, 1024 * 1024)
    if digest(archive) != SOURCE_SHA256 or digest(patch) != PATCH_SHA256:
        raise ValueError("Git bootstrap archive or policy patch does not match its pinned hash")
    # The producer must independently inventory the same canonical policy bytes.
    if digest(read_regular(ROOT / "scripts/git-runtime/policy.patch", 1024 * 1024)) != PATCH_SHA256:
        raise ValueError("Canonical Git policy patch does not match its pinned hash")
    if output.exists() or output.is_symlink():
        raise ValueError("Git bootstrap output must not already exist")
    cache_root.parent.mkdir(parents=True, exist_ok=True)
    private_directory(cache_root)
    native_cache = cache_root / target
    private_directory(native_cache)
    env = build_environment()
    key = SOURCE_SHA256 + "-" + PATCH_SHA256 + "-" + digest(Path(__file__).read_bytes())[:16] + "-" + toolchain_identity(env, target)
    cached = native_cache / key
    producer = prepare_module()
    with cache_lock(native_cache / ".build.lock"):
        if cached.is_symlink():
            raise ValueError("Git bootstrap cache entry cannot be a symlink")
        if not cached.exists():
            staging = Path(tempfile.mkdtemp(prefix=".building-", dir=native_cache))
            try:
                source = extract_source(archive, staging)
                patch_file = staging / "policy.patch"
                patch_file.write_bytes(patch)
                # Actual HOME is preserved; inherited Git policy/config cannot
                # redirect the source build or the version verification.
                run(["patch", "--batch", "--fuzz=0", "-p1", "-i", str(patch_file)], source, env)
                if "musl" in target:
                    namespace_static_error(source)
                run(make_arguments(target, jobs, env), source, env)
                for name in ("git", "git-remote-http"):
                    verify_binary(source / name, target)
                run([str(source / "git"), "--version"], source, env)
                if "apple" in target:
                    # Shipping a Homebrew dylib path would make the bundle host-specific.
                    for name in ("git", "git-remote-http"):
                        dependencies = subprocess.check_output(["otool", "-L", str(source / name)], text=True, timeout=30).splitlines()[1:]
                        if any(not line.strip().split(" ", 1)[0].startswith(("/usr/lib/", "/System/Library/")) for line in dependencies):
                            raise ValueError("Git runtime has a non-system dynamic library dependency")
                producer.prepare(root=ROOT, target=target, source_archive=source_archive, git=source / "git", http_helper=source / "git-remote-http", https_helper=None, libraries=[], output=staging / "bundle")
                validate_bundle(staging / "bundle", target)
                # Retain only the small validated bundle; sources were exclusively owned.
                shutil.rmtree(source)
                patch_file.unlink()
                staging.rename(cached)
            finally:
                if staging.exists():
                    shutil.rmtree(staging)
        validate_bundle(cached / "bundle", target)
        producer.prepare(root=ROOT, target=target, source_archive=source_archive, git=cached / "bundle/git", http_helper=cached / "bundle/git-remote-http", https_helper=cached / "bundle/git-remote-https", libraries=[], output=output)
    return output.absolute()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True, choices=TARGETS)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--output", type=Path)
    mode.add_argument("--print-cache-identity", action="store_true")
    parser.add_argument("--source-archive", type=Path, default=ROOT / "scripts/git-runtime/git-2.56.0.tar.xz")
    parser.add_argument("--policy-patch", type=Path, default=ROOT / "scripts/git-runtime/policy.patch")
    parser.add_argument("--cache-root", type=Path, default=ROOT / "target/git-runtime-bootstrap")
    parser.add_argument("--jobs", type=int, default=min(os.cpu_count() or 1, 8))
    args = parser.parse_args()
    # SIGTERM must retire the owned C build before staging cleanup, too.
    def interrupt(_signum, _frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, interrupt)
    try:
        if args.print_cache_identity:
            require_target(args.target)
            inputs: list[Path] = []
            identity = toolchain_identity(build_environment(), args.target, inputs)
            inputs = list(dict.fromkeys(inputs))
            if len(inputs) > 128 or any(not path.is_absolute() or any(byte in str(path) for byte in ("\r", "\n")) for path in inputs):
                raise ValueError("Git toolchain input paths are not representable")
            output = identity + "\n" + "".join(str(path) + "\n" for path in inputs)
            if len(output.encode()) > 65536:
                raise ValueError("Git toolchain input inventory exceeds its bound")
            print(output, end="")
        else:
            parameters = vars(args).copy()
            parameters.pop("print_cache_identity")
            print(bootstrap(**parameters))
    except (OSError, ValueError, subprocess.CalledProcessError, tarfile.TarError) as error:
        parser.exit(1, f"Git runtime bootstrap failed: {error}\n")


if __name__ == "__main__":
    main()
