#!/usr/bin/env python3
"""Capture exact Alpine/MSYS2 Git dependency sources and verbatim notices.

Publisher-only tooling. Packages must already be installed by the signed distro
package manager. No recipe code is executed, no packages/accounts are changed.
"""
from __future__ import annotations

import argparse
import contextlib
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import re
import shutil
import shlex
import stat
import subprocess
import tarfile
import tempfile
import urllib.error
import urllib.parse
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
MAX_SOURCE = 256 * 1024 * 1024
CURL_SHA256 = "8dadd7d2536a314f982ad05a651b713f426bf7d02cc2c76cffb9796f2218e6c5"
SAFE = re.compile(r"[A-Za-z0-9_][A-Za-z0-9_.+-]{0,160}")
NOTICE = re.compile(r"^(?:licen[cs]e|copying|copyright|notice|unlicense)(?:[-._].*)?$", re.I)
SOURCE_CACHE: Path | None = None


class CollectionError(ValueError):
    pass


def safe(value: str) -> str:
    if not SAFE.fullmatch(value) or ".." in value:
        raise CollectionError(f"unsafe distribution package/source name: {value[:180]!r}")
    return value


def capture(command: list[str]) -> str:
    with tempfile.TemporaryFile() as output, tempfile.TemporaryFile() as errors:
        result = subprocess.run(command, stdout=output, stderr=errors, timeout=120, check=False)
        if result.returncode:
            if len(command) == 4 and Path(command[0]).name == "pacman-key" and command[1] == "--verify":
                # Fixed publisher-only signature verification has no credential
                # arguments. Retain bounded, escaped GPG/keyring diagnostics;
                # other metadata commands keep their closed error category.
                errors.seek(0)
                reason = errors.read(4096).decode("utf-8", errors="replace")
                names = [Path(argument).name for argument in command[2:]]
                raise CollectionError(
                    "MSYS2 package signature verification failed: "
                    + json.dumps({"exit": result.returncode, "files": names, "diagnostic": reason}, ensure_ascii=True)
                )
            raise CollectionError(f"required distro metadata/signature command failed: {command[0]}")
        output.seek(0)
        data = output.read(2 * 1024 * 1024 + 1)
    if len(data) > 2 * 1024 * 1024:
        raise CollectionError("distro metadata exceeds bound")
    return data.decode("utf-8")


def copy_bounded(source: Path, destination: Path) -> None:
    created = False
    try:
        with source.open("rb") as reader, destination.open("xb") as writer:
            created = True
            total = 0
            while block := reader.read(1024 * 1024):
                total += len(block)
                if total > MAX_SOURCE:
                    raise CollectionError("native source copy exceeds bound")
                writer.write(block)
    except BaseException:
        if created:
            destination.unlink(missing_ok=True)
        raise


def fetch(url: str, destination: Path, expected: str | None = None, algorithm: str = "sha512") -> None:
    if urllib.parse.urlsplit(url).scheme != "https":
        raise CollectionError("source downloads require HTTPS")
    digest = hashlib.new(algorithm)
    cached = SOURCE_CACHE / (algorithm + "-" + expected) if SOURCE_CACHE is not None and expected else None
    if cached is not None and cached.exists():
        info = cached.lstat()
        if not stat.S_ISREG(info.st_mode) or info.st_size > MAX_SOURCE:
            raise CollectionError("invalid native source cache entry")
        with cached.open("rb") as stream:
            if hashlib.file_digest(stream, algorithm).hexdigest() != expected:
                raise CollectionError("native source cache checksum mismatch")
        copy_bounded(cached, destination)
        with destination.open("rb") as stream:
            if hashlib.file_digest(stream, algorithm).hexdigest() != expected:
                destination.unlink()
                raise CollectionError("native source cache changed during capture")
        return
    created = False
    try:
        with urllib.request.urlopen(url, timeout=60) as response, destination.open("xb") as output:
            created = True
            if urllib.parse.urlsplit(response.url).scheme != "https":
                raise CollectionError("source redirect left HTTPS")
            total = 0
            while block := response.read(1024 * 1024):
                total += len(block)
                if total > MAX_SOURCE:
                    raise CollectionError("native source download exceeds bound")
                digest.update(block)
                output.write(block)
        if expected and digest.hexdigest() != expected:
            raise CollectionError("distro-pinned source checksum mismatch")
        if cached is not None:
            try:
                copy_bounded(destination, cached)
            except FileExistsError:
                pass  # Another collector's bytes are checked on the next use.
    except BaseException:
        if created:
            destination.unlink(missing_ok=True)
        raise


@contextlib.contextmanager
def archive(path: Path):
    """Read archive headers without extracting any source-selected pathname."""
    with tempfile.TemporaryDirectory(prefix="mini-agent-native-source-read-") as temporary:
        raw = path
        if path.name.endswith(".zst"):
            raw = Path(temporary) / "source.tar"
            child = subprocess.Popen(["zstd", "--decompress", "--stdout", str(path)], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
            try:
                total = 0
                with raw.open("xb") as output:
                    while block := child.stdout.read(1024 * 1024):
                        total += len(block)
                        if total > 1024 * 1024 * 1024:
                            raise CollectionError("expanded source package exceeds bound")
                        output.write(block)
                if child.wait(timeout=30):
                    raise CollectionError("cannot decompress distro source package")
            finally:
                child.stdout.close()
                if child.poll() is None:
                    child.kill()
                child.wait()
        with tarfile.open(raw, "r:*") as source:
            members = source.getmembers()
            if len(members) > 200000:
                raise CollectionError("source archive member count exceeds bound")
            yield source, members


def member_bytes(source, member, limit=2 * 1024 * 1024):
    if not member.isfile() or member.size > limit:
        raise CollectionError("required source metadata is not a bounded regular member")
    stream = source.extractfile(member)
    with stream:
        data = stream.read(limit + 1)
    if len(data) > limit:
        raise CollectionError("source member exceeds bound")
    return data


def asset(path: Path, root: Path) -> dict:
    with path.open("rb") as stream:
        h = hashlib.file_digest(stream, "sha256").hexdigest()
    return {"path": path.relative_to(root).as_posix(), "sha256": h}


def notices_from_archive(path: Path, destination: Path) -> list[Path]:
    notices = []
    with archive(path) as (source, members):
        for member in members:
            parts = Path(member.name).parts
            # Include upstream/embedded-library notices, omit test fixtures.
            if len(parts) > 5 or any(p in {"tests", "test", "testsuite", "examples"} for p in parts) or not NOTICE.fullmatch(Path(member.name).name) or not member.isfile():
                continue
            data = member_bytes(source, member, 1024 * 1024)
            if len(notices) >= 128:
                raise CollectionError("too many native source notices")
            output = destination / f"{len(notices):03d}-{safe(Path(member.name).name)}"
            output.write_bytes(data)
            notices.append(output)
    return notices


def alpine_packages(database: Path) -> dict[str, dict[str, str]]:
    with database.open("rb") as stream:
        raw = stream.read(32 * 1024 * 1024 + 1)
    if len(raw) > 32 * 1024 * 1024:
        raise CollectionError("Alpine installed metadata exceeds bound")
    result = {}
    for paragraph in raw.decode().split("\n\n"):
        fields = {}
        for line in paragraph.splitlines():
            if len(line) > 2 and line[1] == ":" and line[0] in {"P", "V", "o", "c", "L", "U"}:
                if line[0] in fields:
                    raise CollectionError("duplicate Alpine package identity field")
                fields[line[0]] = line[2:]
        if "P" in fields:
            result[fields["P"]] = fields
    return result



def static_closure(installed: dict[str, dict[str, str]], *, custom_curl: Path = Path("/opt/mini-agent-curl/lib/libcurl.a")) -> tuple[set[str], dict]:
    """Capture the actual curl/GCC static inputs, not a predicted package list."""
    if capture(["pkg-config", "--modversion", "libcurl"]).strip() != "8.23.0":
        raise CollectionError("static closure requires the pinned custom curl version")
    flags = shlex.split(capture(["pkg-config", "--libs", "--static", "libcurl"]))
    directories = [Path(flag[2:]) for flag in flags if flag.startswith("-L")]
    libraries = {flag[2:] for flag in flags if flag.startswith("-l")}
    if any(not (flag.startswith("-L") or flag.startswith("-l") or flag == "-pthread") for flag in flags):
        raise CollectionError("unsupported static linker flag; update the explicit source collector")
    if "curl" not in libraries:
        raise CollectionError("static closure omits curl")
    libraries.update({"c", "gcc", "gcc_eh"})
    identities = {name + "-" + fields["V"]: name for name, fields in installed.items()}
    packages, inputs = set(), []
    for library in sorted(libraries):
        safe(library)
        filename = "lib" + library + ".a"
        candidates = [directory / filename for directory in directories if (directory / filename).is_file()]
        if candidates:
            path = candidates[0].resolve(strict=True)
        else:
            compiler_path = capture(["cc", "-print-file-name=" + filename]).strip()
            if compiler_path == filename:
                raise CollectionError("static linker input is unavailable")
            path = Path(compiler_path).resolve(strict=True)
        if not path.is_file() or path.stat().st_size > MAX_SOURCE:
            raise CollectionError("invalid static linker archive")
        if library == "curl":
            if path != custom_curl:
                raise CollectionError("static curl must be the source-built pinned archive")
            owner = "custom-curl-8.23.0"
        else:
            ownership = capture(["apk", "info", "--who-owns", str(path)]).strip()
            owner = ownership.removeprefix(str(path) + " is owned by ")
            if owner not in identities:
                raise CollectionError("static linker archive lacks an exact installed package owner")
            packages.add(identities[owner])
        with path.open("rb") as stream:
            digest = hashlib.file_digest(stream, "sha256").hexdigest()
        inputs.append({"path": str(path), "sha256": digest, "owner": owner})
    return packages, {"flags": flags, "archives_and_compiler_defaults": inputs}


def apk_checksums(recipe: str) -> list[tuple[str, str]]:
    match = re.search(r'(?ms)^sha512sums="\s*\n?(.*?)"', recipe)
    if not match:
        raise CollectionError("APKBUILD has no literal SHA512 source inventory")
    result = []
    for line in match[1].strip().splitlines():
        fields = line.split()
        if len(fields) != 2 or not re.fullmatch(r"[a-f0-9]{128}", fields[0]):
            raise CollectionError("APKBUILD checksum inventory requires unsupported evaluation")
        result.append((fields[0], safe(fields[1])))
    if not result or len(result) > 128:
        raise CollectionError("invalid Alpine source count")
    return result


def collect_alpine(name: str, fields: dict, output: Path, series: str) -> tuple[str, str, str, list[Path], list[Path]]:
    origin, version, commit = safe(fields["o"]), safe(fields["V"]), fields["c"]
    if not re.fullmatch(r"[a-f0-9]{40}", commit) or not re.fullmatch(r"v[0-9]+\.[0-9]+", series):
        raise CollectionError("Alpine source requires exact signed-package aports commit and series")
    recipe_path = output / "APKBUILD"
    base = None
    for repository in ("main", "community", "testing"):
        candidate = f"https://raw.githubusercontent.com/alpinelinux/aports/{commit}/{repository}/{origin}/"
        try:
            fetch(candidate + "APKBUILD", recipe_path)
            base = candidate
            break
        except urllib.error.HTTPError as error:
            if error.code != 404:
                raise
    if base is None:
        raise CollectionError("exact Alpine source recipe is unavailable")
    sources = [recipe_path]
    notices = []
    for expected, filename in apk_checksums(recipe_path.read_text()):
        destination = output / filename
        try:
            fetch(f"https://distfiles.alpinelinux.org/distfiles/{series}/{filename}", destination, expected)
        except (urllib.error.HTTPError, CollectionError):
            # Recipe-local patches are not in the distfiles mirror. Their
            # checksum must still match the exact APKBUILD from installed data.
            fetch(base + urllib.parse.quote(filename), destination, expected)
        sources.append(destination)
        if tarfile.is_tarfile(destination):
            nested = output / (filename + "-notices")
            nested.mkdir()
            notices += notices_from_archive(destination, nested)
        elif NOTICE.fullmatch(filename):
            notices.append(destination)
    (output / "aports-commit.txt").write_text(commit + "\n")
    sources.append(output / "aports-commit.txt")
    if not notices:
        raise CollectionError(f"no verbatim source license found for Alpine origin {origin}")
    return version, fields["L"], base, sources, notices


def assignments(text: str) -> dict[str, str]:
    values = {}
    for line in text.splitlines():
        key, sep, value = line.partition(" = ")
        if sep:
            if key in values and key in {"pkgname", "pkgbase", "pkgver", "pkgbuild_sha256sum"}:
                raise CollectionError("duplicate signed package identity")
            if key == "license" and key in values:
                values[key] += " ; " + value  # preserve declarations, infer no license choice
            else:
                values[key] = value
    return values


def collect_msys2(name: str, output: Path, cache: Path) -> tuple[str, str, str, list[Path], list[Path]]:
    identity = capture(["pacman", "-Q", name]).strip().split()
    if len(identity) != 2 or identity[0] != name:
        raise CollectionError("MSYS2 installed package identity mismatch")
    version = safe(identity[1])
    candidates = list(cache.glob(safe(name) + "-" + version + "-*.pkg.tar.zst"))
    if len(candidates) != 1:
        raise CollectionError("retain the exact installed MSYS2 package in pacman cache")
    binary = candidates[0]
    signature = Path(str(binary) + ".sig")
    if not signature.is_file():
        fetch("https://repo.msys2.org/mingw/ucrt64/" + binary.name + ".sig", output / "binary.sig")
        signature = output / "binary.sig"
    capture(["pacman-key", "--verify", str(signature), str(binary)])
    with archive(binary) as (source, members):
        by_name = {m.name.lstrip("./"): m for m in members}
        build = member_bytes(source, by_name["BUILDINFO"] if "BUILDINFO" in by_name else next(m for m in members if m.name == ".BUILDINFO"))
        info = member_bytes(source, next(m for m in members if m.name == ".PKGINFO"))
    build_fields, package_fields = assignments(build.decode()), assignments(info.decode())
    if package_fields.get("pkgname") != name or package_fields.get("pkgver") != identity[1]:
        raise CollectionError("signed cached package does not match installed package")
    expected_recipe = build_fields.get("pkgbuild_sha256sum", "")
    if not re.fullmatch(r"[a-f0-9]{64}", expected_recipe):
        raise CollectionError("signed MSYS2 build metadata lacks PKGBUILD digest")
    pkgbase = safe(build_fields["pkgbase"])
    source_url = f"https://repo.msys2.org/mingw/sources/{pkgbase}-{version}.src.tar.zst"
    source_package = output / "source.src.tar.zst"
    fetch(source_url, source_package)
    sources, notices = [source_package], []
    (output / "BUILDINFO").write_bytes(build)
    (output / "PKGINFO").write_bytes(info)
    sources += [output / "BUILDINFO", output / "PKGINFO"]
    with archive(source_package) as (source, members):
        recipe = next(m for m in members if Path(m.name).name == "PKGBUILD")
        recipe_bytes = member_bytes(source, recipe)
        if hashlib.sha256(recipe_bytes).hexdigest() != expected_recipe:
            raise CollectionError("MSYS2 source recipe does not match signed binary build")
        recipe_text = recipe_bytes.decode()
        checksum_blocks = re.findall(r"(?ms)^(sha256sums|sha512sums)\s*=\s*\((.*?)\)", recipe_text)
        expected = set()
        for algorithm, block in checksum_blocks:
            words = shlex.split(block, comments=True)
            size = 64 if algorithm == "sha256sums" else 128
            if not words or any(not re.fullmatch("[a-f0-9]{" + str(size) + "}", word) for word in words):
                raise CollectionError("MSYS2 checksum inputs require unsupported recipe evaluation")
            expected.update((algorithm.replace("sums", ""), word) for word in words)
        if not expected:
            raise CollectionError("MSYS2 source checksums require unsupported recipe evaluation")
        found = set()
        for member in members:
            if not member.isfile() or Path(member.name).name in {"PKGBUILD", ".SRCINFO"}:
                continue
            data = member_bytes(source, member, MAX_SOURCE)
            matches = {(algorithm, value) for algorithm, value in expected if hashlib.new(algorithm, data).hexdigest() == value}
            found |= matches
            if matches:
                captured = output / ("verified-" + safe(Path(member.name).name))
                captured.write_bytes(data)
                if tarfile.is_tarfile(captured):
                    directory = output / (captured.name + "-notices")
                    directory.mkdir()
                    notices += notices_from_archive(captured, directory)
                elif NOTICE.fullmatch(Path(member.name).name):
                    notices.append(captured)
        if expected != found:
            raise CollectionError("MSYS2 full-source archive omits checksum-pinned recipe inputs")
    if not notices:
        raise CollectionError("MSYS2 source archive supplies no verbatim license notices")
    return version, package_fields.get("license", "see verbatim upstream notices"), source_url, sources, notices


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--distro", choices=["alpine", "msys2"], required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--bundle", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--package", action="append", default=[])
    parser.add_argument("--static-curl-closure", action="store_true", help="derive actual Alpine static link owners from pkg-config and the compiler")
    parser.add_argument("--alpine-db", type=Path, default=Path("/lib/apk/db/installed"))
    parser.add_argument("--alpine-series", help="default: derive vMAJOR.MINOR from the actual /etc/alpine-release")
    parser.add_argument("--pacman-cache", type=Path, default=Path("/var/cache/pacman/pkg"))
    parser.add_argument("--toolchain-prefix", type=Path, default=Path("/ucrt64"))
    parser.add_argument("--curl-source", type=Path)
    parser.add_argument("--source-cache", type=Path, help="reuse only checksum-verified source files across collection retries")
    args = parser.parse_args()
    created = False
    successful = False
    try:
        global SOURCE_CACHE
        if args.source_cache:
            args.source_cache.mkdir(mode=0o700, parents=True, exist_ok=True)
            if args.source_cache.is_symlink() or not args.source_cache.is_dir():
                raise CollectionError("invalid native source cache directory")
            info = args.source_cache.stat()
            if info.st_uid != os.getuid() or info.st_mode & 0o077:
                raise CollectionError("native source cache must be an owned private directory")
            SOURCE_CACHE = args.source_cache
        if args.distro == "alpine" and not args.alpine_series:
            release = Path("/etc/alpine-release").read_text().strip().split(".")
            if len(release) < 2 or not all(part.isdigit() for part in release[:2]):
                raise CollectionError("cannot derive actual Alpine source series")
            args.alpine_series = "v" + ".".join(release[:2])
        manifest = (args.bundle / "bundle.manifest").read_bytes()
        if len(manifest) > 65536 or f"target={args.target}" not in manifest.decode().splitlines()[:5]:
            raise CollectionError("native bundle/target mismatch")
        files = {}
        for line in manifest.decode().splitlines()[5:]:
            name, sha, mode = line.split("\t")
            if mode == "r":
                leaf = safe(name.removeprefix("file="))
                source = args.toolchain_prefix / "bin" / leaf
                if hashlib.sha256(source.read_bytes()).hexdigest() != sha:
                    raise CollectionError("installed DLL differs from captured runtime")
                owner = capture(["pacman", "-Qqo", str(source)]).strip()
                files.setdefault(safe(owner), []).append({"name": leaf, "sha256": sha})
        installed = alpine_packages(args.alpine_db) if args.distro == "alpine" else {}
        names = set(args.package) | set(files)
        closure = None
        if args.static_curl_closure:
            if args.distro != "alpine" or not args.curl_source:
                raise CollectionError("static closure requires Alpine and the pinned curl source")
            derived, closure = static_closure(installed)
            names.update(derived)
        if not names and not args.curl_source:
            raise CollectionError("actual native dependency package closure is required")
        args.output.mkdir(mode=0o700)
        created = True
        components = []
        origins = {}
        if closure is not None:
            (args.output / "static-link-inputs.json").write_text(json.dumps(closure, indent=2) + "\n")
        for name in sorted(names):
            directory = args.output / safe(name)
            directory.mkdir()
            if args.distro == "alpine":
                fields = installed[name]
                identity = (fields["o"], fields["V"], fields["c"])
                if identity not in origins:
                    origins[identity] = collect_alpine(name, fields, directory, args.alpine_series)
                version, _, url, sources, notices = origins[identity]
                license = fields["L"]
            else:
                version, license, url, sources, notices = collect_msys2(name, directory, args.pacman_cache)
            components.append({"name": name, "version": version, "license": license, "source_url": url, "bundled_files": files.get(name, []), "license_files": [asset(p, args.output) for p in notices], "source_files": [asset(p, args.output) for p in sources]})
        if args.curl_source:
            with args.curl_source.open("rb") as stream:
                if hashlib.file_digest(stream, "sha256").hexdigest() != CURL_SHA256:
                    raise CollectionError("custom curl source differs from pinned 8.23.0")
            directory = args.output / "curl-8.23.0"
            directory.mkdir()
            source = directory / "curl-8.23.0.tar.xz"
            copy_bounded(args.curl_source, source)
            notices = notices_from_archive(source, directory)
            if not notices:
                raise CollectionError("pinned curl source lacks verbatim notice")
            components.append({"name": "curl", "version": "8.23.0", "license": "curl", "source_url": "https://curl.se/download/curl-8.23.0.tar.xz", "bundled_files": [], "license_files": [asset(p, args.output) for p in notices], "source_files": [asset(source, args.output)]})
        if closure is not None:
            for component in components:
                component["source_files"].append(asset(args.output / "static-link-inputs.json", args.output))
        record = {"format": "mini-agent-native-components/1", "target": args.target, "bundle_manifest_sha256": hashlib.sha256(manifest).hexdigest(), "components": components}
        path = args.output / "native-components.json"
        path.write_text(json.dumps(record, indent=2) + "\n")
        spec = importlib.util.spec_from_file_location("native_inventory", ROOT / "scripts/third_party_licenses.py")
        inventory = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(inventory)
        inventory.load_native_record(path, args.target, args.bundle)
        print(path.absolute())
        successful = True
    except (OSError, ValueError, KeyError, StopIteration, tarfile.TarError, subprocess.SubprocessError) as error:
        parser.exit(1, f"Native Git source collection failed: {error}\n")
    finally:
        if created and not successful:
            shutil.rmtree(args.output)


if __name__ == "__main__":
    main()
