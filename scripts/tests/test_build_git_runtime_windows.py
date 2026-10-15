import importlib.util
import io
import json
import os
from pathlib import Path
import struct
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch


SCRIPT = Path(__file__).resolve().parents[1] / "build-git-runtime-windows.py"
SPEC = importlib.util.spec_from_file_location("windows_git_bootstrap", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


def pe_image(imports=(), delayed=(), forwarded=()):
    data = bytearray(4096)
    data[:2] = b"MZ"
    struct.pack_into("<I", data, 0x3C, 0x80)
    data[0x80:0x84] = b"PE\0\0"
    struct.pack_into("<HHIIIHH", data, 0x84, 0x8664, 1, 0, 0, 0, 240, 0)
    optional = 0x98
    struct.pack_into("<H", data, optional, 0x20B)
    struct.pack_into("<I", data, optional + 108, 16)
    struct.pack_into("<8sIIII", data, optional + 240, b".rdata", 3584, 0x1000, 3584, 512)

    def offset(rva):
        return 512 + rva - 0x1000

    name_rva = 0x1800

    def name(value):
        nonlocal name_rva
        result = name_rva
        encoded = value.encode("ascii") + b"\0"
        data[offset(result):offset(result) + len(encoded)] = encoded
        name_rva += len(encoded)
        return result

    for directory, table, width, names, name_offset in ((1, 0x1000, 20, imports, 12), (13, 0x1100, 32, delayed, 4)):
        if names:
            struct.pack_into("<II", data, optional + 112 + directory * 8, table, width * (len(names) + 1))
            for index, value in enumerate(names):
                entry = offset(table + index * width)
                if directory == 13:
                    struct.pack_into("<I", data, entry, 1)
                struct.pack_into("<I", data, entry + name_offset, name(value))
    if forwarded:
        struct.pack_into("<II", data, optional + 112, 0x1200, 0x900)
        struct.pack_into("<I", data, offset(0x1200) + 20, len(forwarded))
        struct.pack_into("<I", data, offset(0x1200) + 28, 0x1300)
        for index, value in enumerate(forwarded):
            struct.pack_into("<I", data, offset(0x1300) + index * 4, name(value))
    return bytes(data)


class WindowsGitBootstrapTests(unittest.TestCase):
    def setUp(self):
        owner = tempfile.TemporaryDirectory(prefix="mini-agent-git-windows-test-")
        self.addCleanup(owner.cleanup)
        self.root = Path(owner.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()

    def read(self, path, limit):
        data = path.read_bytes()
        self.assertLessEqual(len(data), limit)
        return data

    def file(self, name, imports=(), delayed=(), forwarded=()):
        path = self.bin / name
        path.write_bytes(pe_image(imports, delayed, forwarded))
        return path

    def native_acl(self):
        user = "S-1-5-21-100-200-300-1001"
        return {"owner": user, "user": user, "protected": True, "rules": [
            {"sid": sid, "rights": 0x1F01FF, "type": 0, "inherited": False, "inheritance": 3, "propagation": 0}
            for sid in (user, "S-1-5-18", "S-1-5-32-544")
        ]}

    def test_native_private_cache_accepts_noacl_stat_bits_without_relaxing_dacl(self):
        directory = self.root / "native cache with spaces"
        directory.mkdir(mode=0o777)
        directory.chmod(0o777)
        # Reproduce the observed decision boundary: an owned native-private
        # directory can expose broad synthetic MSYS noacl permission bits.
        with self.assertRaisesRegex(ValueError, "owned private"):
            MODULE.common_module().private_directory(directory)
        environment = {"SystemRoot": "C:/Windows", "HOME": os.environ.get("HOME", "")}
        response = subprocess.CompletedProcess([], 0, json.dumps(self.native_acl()).encode(), b"")
        with patch.object(MODULE.subprocess, "check_output", side_effect=["C:\\private cache\\directory\n", "/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe\n"]), patch.object(MODULE.subprocess, "run", return_value=response) as native:
            MODULE.private_directory(directory, environment)
        command = native.call_args.args[0]
        self.assertEqual(command[0], "/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe")
        self.assertEqual(command[1:5], ["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"])
        self.assertIn("CreateDirectory($path, $acl)", command[5])
        self.assertIn("GetAccessControl($path)", command[5])
        self.assertIn("ReparsePoint", command[5])
        self.assertTrue(native.call_args.kwargs["check"])
        self.assertEqual(native.call_args.kwargs["timeout"], 30)
        child = native.call_args.kwargs["env"]
        self.assertEqual(child["MINI_AGENT_GIT_BUILD_DIRECTORY"], "C:\\private cache\\directory")
        self.assertEqual(child["HOME"], environment["HOME"])
        self.assertNotIn("MINI_AGENT_GIT_BUILD_DIRECTORY", environment)

    def test_native_cache_rejects_wrong_owner_broad_or_inherited_rules_and_symlinks(self):
        mutations = [
            lambda value: value.update(owner="S-1-5-32-544"),
            lambda value: value.update(protected=False),
            lambda value: value["rules"][0].update(sid="S-1-1-0"),
            lambda value: value["rules"][0].update(inherited=True),
            lambda value: value["rules"][0].update(rights=0x120089),
            lambda value: value["rules"][0].update(type=1),
            lambda value: value["rules"].append(value["rules"][0].copy()),
        ]
        for mutate in mutations:
            record = self.native_acl()
            mutate(record)
            with self.assertRaises(ValueError):
                MODULE.validate_cache_acl(record)
        directory = self.root / "cache"
        directory.mkdir()
        link = self.root / "cache-link"
        link.symlink_to(directory, target_is_directory=True)
        with patch.object(MODULE.subprocess, "run") as native, self.assertRaisesRegex(ValueError, "real directory"):
            MODULE.private_directory(link, {"SystemRoot": "C:/Windows"})
        native.assert_not_called()

    def test_pe_imports_include_delay_and_export_forwarding(self):
        data = pe_image(["KERNEL32.dll", "libcurl-4.dll"], ["optional.dll"], ["forwarded.target", "other.dll.#1"])
        self.assertEqual(MODULE.pe_dependencies(data), {"kernel32.dll", "libcurl-4.dll", "optional.dll", "forwarded.dll", "other.dll"})

    def test_pe_malformed_architecture_and_import_paths_fail_closed(self):
        values = [b"not PE", pe_image(["../outside.dll"])]
        wrong = bytearray(pe_image())
        struct.pack_into("<H", wrong, 0x84, 0x14C)
        values.append(bytes(wrong))
        delayed_va = bytearray(pe_image(delayed=["test.dll"]))
        struct.pack_into("<I", delayed_va, 512 + 0x100, 0)
        values.append(bytes(delayed_va))
        missing_end = bytearray(pe_image(["test.dll"]))
        struct.pack_into("<I", missing_end, 0x98 + 112 + 8 + 4, 20)
        values.append(bytes(missing_end))
        for data in values:
            with self.subTest(data=data[:8]), self.assertRaises(ValueError):
                MODULE.pe_dependencies(data)

    def test_complete_transitive_dll_closure_and_cycles(self):
        git = self.file("git.exe", ["KERNEL32.dll", "liba.dll"])
        http = self.file("http.exe", ["libcurl-4.dll"])
        self.file("liba.dll", ["libb.dll", "api-ms-win-crt-runtime-l1-1-0.dll"])
        self.file("libb.dll", ["liba.dll", "ws2_32.dll"])
        self.file("libcurl-4.dll", delayed=["delayed.dll"], forwarded=["forwarded.target"])
        self.file("delayed.dll")
        self.file("forwarded.dll")
        libraries = MODULE.dependency_closure([git, http], self.bin, self.read)
        self.assertEqual([p.name for p in libraries], ["delayed.dll", "forwarded.dll", "liba.dll", "libb.dll", "libcurl-4.dll"])

    def test_missing_dll_and_msys_runtime_never_use_path_fallback(self):
        outside = self.root / "outside"
        outside.mkdir()
        (outside / "missing.dll").write_bytes(pe_image())
        git = self.file("git.exe", ["missing.dll"])
        with patch.dict(os.environ, {"PATH": str(outside)}), self.assertRaisesRegex(ValueError, "Missing explicitly"):
            MODULE.dependency_closure([git], self.bin, self.read)
        for name in ["msys-2.0.dll", "cygwin1.dll"]:
            git.write_bytes(pe_image([name]))
            self.file(name)
            with self.assertRaisesRegex(ValueError, "MSYS/Cygwin"):
                MODULE.dependency_closure([git], self.bin, self.read)

    def test_dependency_case_collision_and_wrong_architecture_refuse(self):
        git = self.file("git.exe", ["test.dll"])
        dll = self.file("test.dll")
        wrong = bytearray(dll.read_bytes())
        struct.pack_into("<H", wrong, 0x84, 0x14C)
        dll.write_bytes(wrong)
        with self.assertRaisesRegex(ValueError, "AMD64"):
            MODULE.dependency_closure([git], self.bin, self.read)
        # File systems with case-insensitive names cannot create this fixture.
        other = self.bin / "TEST.dll"
        other.write_bytes(pe_image())
        if len([p for p in self.bin.iterdir() if p.suffix.lower() == ".dll"]) == 2:
            with self.assertRaisesRegex(ValueError, "Case-colliding"):
                MODULE.dependency_closure([git], self.bin, self.read)

    def test_toolchain_requires_posix_ucrt64_native_compiler_and_schannel(self):
        with patch.object(MODULE.sys, "platform", "linux"), self.assertRaisesRegex(ValueError, "MSYS2"):
            MODULE.toolchain_environment(Path("/ucrt64"))
        with patch.object(MODULE.sys, "platform", "msys"), patch.dict(os.environ, {"MSYSTEM": "UCRT64", "GIT_EXEC_PATH": "bad", "MINI_AGENT_GIT_GLOBAL_SNAPSHOT": "bad"}):
            with patch.object(MODULE.subprocess, "check_output", side_effect=["x86_64-w64-mingw32\n", "Schannel\n"]):
                env = MODULE.toolchain_environment(Path("/ucrt64"))
                self.assertEqual(env.get("HOME"), os.environ.get("HOME"))
                self.assertFalse(any(key.startswith("GIT_") for key in env))
                self.assertEqual(env["PATH"], "/ucrt64/bin:/usr/bin:/bin")
            for outputs in [["i686-w64-mingw32\n"], ["x86_64-w64-mingw32\n", "OpenSSL Schannel\n"]]:
                with patch.object(MODULE.subprocess, "check_output", side_effect=outputs), self.assertRaises(ValueError):
                    MODULE.toolchain_environment(Path("/ucrt64"))

    def test_build_publishes_entire_dll_closure_and_cleans_version_failure(self):
        common = MODULE.common_module()
        producer = common.prepare_module()
        source_bytes = io.BytesIO()
        with tarfile.open(fileobj=source_bytes, mode="w:xz") as archive:
            member = tarfile.TarInfo("git-2.56.0/source.c")
            member.size = 7
            archive.addfile(member, io.BytesIO(b"fixture"))
        archive = self.root / "source.tar.xz"
        archive.write_bytes(source_bytes.getvalue())
        policy = self.root / "scripts/git-runtime/policy.patch"
        policy.parent.mkdir(parents=True)
        policy.write_bytes(b"owned fixture policy")
        self.file("libcurl-4.dll", ["kernel32.dll"])
        self.file("libiconv-2.dll")
        arguments = dict(source_archive=archive, policy_patch=policy, cache_root=self.root / "cache", toolchain_prefix=self.root, jobs=1)

        def fake_run(command, cwd, env):
            if command[0].endswith("make"):
                self.assertIn("NO_RUST=YesPlease", command)
                self.assertIn("NO_OPENSSL=YesPlease", command)
                (cwd / "git.exe").write_bytes(pe_image(["libiconv-2.dll"]))
                (cwd / "git-remote-http.exe").write_bytes(pe_image(["libcurl-4.dll"]))
            elif command[-1] == "--version":
                self.assertEqual(env["PATH"], str(cwd) + ":/c/Windows/System32:/c/Windows")
                if cwd.name == "failed":
                    raise subprocess.CalledProcessError(1, command)

        with patch.object(MODULE, "ROOT", self.root), patch.object(common, "ROOT", self.root), patch.object(MODULE, "common_module", return_value=common), patch.object(common, "SOURCE_SHA256", common.digest(archive.read_bytes())), patch.object(common, "PATCH_SHA256", common.digest(policy.read_bytes())), patch.object(producer, "SOURCE_SHA256", common.digest(archive.read_bytes())), patch.object(common, "prepare_module", return_value=producer), patch.object(MODULE, "toolchain_environment", return_value={"HOME": os.environ.get("HOME", ""), "SYSTEMROOT": "C:/Windows"}), patch.object(common, "run", side_effect=fake_run), patch.object(MODULE.subprocess, "check_output", return_value="/c/Windows\n"), patch.object(MODULE, "private_directory", side_effect=lambda path, env: path.mkdir(mode=0o700, exist_ok=True)), patch.object(common, "cache_lock", side_effect=AssertionError("fresh Windows builds must not use a POSIX-mode cache lock")):
            output = MODULE.bootstrap(output=self.root / "success", **arguments)
            self.assertEqual({p.name for p in output.iterdir()}, {"git.exe", "git-remote-http.exe", "git-remote-https.exe", "libcurl-4.dll", "libiconv-2.dll", "bundle.manifest"})
            self.assertEqual((output / "libcurl-4.dll").read_bytes(), (self.bin / "libcurl-4.dll").read_bytes())
            self.assertIn("target=x86_64-pc-windows-msvc", (output / "bundle.manifest").read_text())
            with self.assertRaises(subprocess.CalledProcessError):
                MODULE.bootstrap(output=self.root / "failed", **arguments)
            self.assertFalse((self.root / "failed").exists())
        self.assertFalse(list(arguments["cache_root"].rglob("source.c")))
        self.assertFalse(list(arguments["cache_root"].rglob(".windows-building-*")))


if __name__ == "__main__":
    unittest.main()
