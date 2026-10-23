import importlib.util
import io
import os
from pathlib import Path
import struct
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch


SCRIPT = Path(__file__).resolve().parents[1] / "build-git-runtime.py"
SPEC = importlib.util.spec_from_file_location("build_git_runtime", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)
TARGET = "aarch64-apple-darwin"


def archive_bytes(entries):
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w:xz") as archive:
        for name, data, kind in entries:
            member = tarfile.TarInfo(name)
            if kind == "symlink":
                member.type = tarfile.SYMTYPE
                member.linkname = data
                archive.addfile(member)
            elif kind == "hardlink":
                member.type = tarfile.LNKTYPE
                member.linkname = data
                archive.addfile(member)
            else:
                member.size = len(data)
                archive.addfile(member, io.BytesIO(data))
    return output.getvalue()


def macho():
    return b"\xcf\xfa\xed\xfe" + struct.pack("<I", 0x0100000C) + bytes(32)


def elf(program_types=(1,), machine=183):
    data = bytearray(64 + 56 * len(program_types))
    data[:6] = b"\x7fELF\x02\x01"
    struct.pack_into("<H", data, 18, machine)
    struct.pack_into("<Q", data, 32, 64)
    struct.pack_into("<HH", data, 54, 56, len(program_types))
    for index, kind in enumerate(program_types):
        struct.pack_into("<I", data, 64 + 56 * index, kind)
    return data


class GitBootstrapTests(unittest.TestCase):
    def setUp(self):
        owner = tempfile.TemporaryDirectory(prefix="mini-agent-git-bootstrap-test-")
        self.addCleanup(owner.cleanup)
        self.root = Path(owner.name)
        toolchain = self.root / "fixture-toolchain"
        toolchain.mkdir()
        programs = {}
        for name in ("cc", "curl-config", "pkg-config"):
            program = toolchain / name
            program.write_bytes(("fixture program " + name).encode())
            program.chmod(0o700)
            programs[name] = str(program)
        programs["/usr/bin/curl-config"] = programs["curl-config"]
        self.programs = programs
        self.enterContext(patch.object(
            MODULE.shutil, "which", side_effect=lambda name, **_kwargs: programs.get(name)
        ))

    def test_target_plan_refuses_cross_native_musl_and_msvc(self):
        with patch.object(MODULE, "native_target", return_value=TARGET):
            MODULE.require_target(TARGET)
            for target in MODULE.TARGETS:
                if target != TARGET:
                    with self.subTest(target=target), self.assertRaises(ValueError):
                        MODULE.require_target(target)

    def test_extraction_preserves_only_internal_links(self):
        data = archive_bytes([
            ("git-2.56.0/readme", b"source", "file"),
            ("git-2.56.0/sub/readme", "../readme", "symlink"),
        ])
        source = MODULE.extract_source(data, self.root)
        self.assertEqual((source / "sub/readme").read_bytes(), b"source")
        self.assertEqual(os.readlink(source / "sub/readme"), "../readme")

    def test_extraction_rejects_traversal_duplicates_and_link_ancestors(self):
        invalid = [
            [("../escape", b"x", "file")],
            [("/absolute", b"x", "file")],
            [("git-2.56.0/link", "../../escape", "symlink")],
            [("git-2.56.0/link", "/escape", "symlink")],
            [("git-2.56.0/link", "readme", "hardlink")],
            [("git-2.56.0/file", b"a", "file"), ("git-2.56.0/./file", b"b", "file")],
            [("git-2.56.0/link", "other", "symlink"), ("git-2.56.0/link/child", b"x", "file")],
        ]
        for index, entries in enumerate(invalid):
            directory = self.root / str(index)
            directory.mkdir()
            with self.subTest(entries=entries), self.assertRaises(ValueError):
                MODULE.extract_source(archive_bytes(entries), directory)
        self.assertFalse((self.root / "escape").exists())

    def test_binary_architecture_is_not_just_a_target_label(self):
        binary = self.root / "binary"
        binary.write_bytes(macho())
        MODULE.verify_binary(binary, TARGET)
        with self.assertRaisesRegex(ValueError, "native target"):
            MODULE.verify_binary(binary, "x86_64-apple-darwin")
        binary.write_bytes(b"\x7fELF\x02\x01" + bytes(12) + struct.pack("<H", 62))
        MODULE.verify_binary(binary, "x86_64-unknown-linux-gnu")
        with self.assertRaisesRegex(ValueError, "native target"):
            MODULE.verify_binary(binary, "aarch64-unknown-linux-gnu")

    def test_musl_payload_refuses_dynamic_loader_and_malformed_headers(self):
        binary = self.root / "git"
        target = "aarch64-unknown-linux-musl"
        binary.write_bytes(elf())
        MODULE.verify_binary(binary, target)
        for data in [elf((1, 3)), elf((1, 2)), elf()[:64], elf(())]:
            binary.write_bytes(data)
            with self.subTest(size=len(data)), self.assertRaises(ValueError):
                MODULE.verify_binary(binary, target)
        binary.write_bytes(elf(machine=62))
        with self.assertRaises(ValueError):
            MODULE.verify_binary(binary, target)
        # A static PIE may contain self-relocation metadata without a loader.
        pie = elf((1, 2))
        offset = len(pie)
        struct.pack_into("<Q", pie, 64 + 56 + 8, offset)
        struct.pack_into("<Q", pie, 64 + 56 + 32, 32)
        pie.extend(bytes(32))
        binary.write_bytes(pie)
        MODULE.verify_binary(binary, target)
        struct.pack_into("<q", pie, offset, 1)  # DT_NEEDED
        binary.write_bytes(pie)
        with self.assertRaisesRegex(ValueError, "external dynamic dependency"):
            MODULE.verify_binary(binary, target)

    def test_musl_make_uses_full_static_curl_closure_and_no_rust(self):
        env = {"HOME": "preserved"}
        with patch.object(MODULE.subprocess, "check_output", side_effect=["mbedTLS\n", "-lcurl -lmbedtls -lz\n"]) as probe:
            args = MODULE.make_arguments("aarch64-unknown-linux-musl", 2, env)
        self.assertEqual(probe.call_args_list[-1].args[0], ["pkg-config", "--static", "--libs", "libcurl"])
        self.assertIn("LDFLAGS=-static", args)
        self.assertIn("CURL_LDFLAGS=-lcurl -lmbedtls -lz", args)
        self.assertIn("NO_RUST=YesPlease", args)
        with patch.object(MODULE.subprocess, "check_output", side_effect=["mbedTLS", "-lcurl\ninjected"]), self.assertRaises(ValueError):
            MODULE.make_arguments("aarch64-unknown-linux-musl", 1, env)
        with patch.object(MODULE.subprocess, "check_output") as probe:
            MODULE.make_arguments(TARGET, 1, env)
        probe.assert_not_called()
        with patch.object(MODULE.subprocess, "check_output", return_value="OpenSSL"), self.assertRaisesRegex(ValueError, "Mbed TLS"):
            MODULE.make_arguments("aarch64-unknown-linux-musl", 1, env)

    def test_native_musl_target_does_not_enable_cross_compilation(self):
        with patch.object(MODULE, "native_target", return_value="aarch64-unknown-linux-musl"):
            MODULE.require_target("aarch64-unknown-linux-musl")
            for target in ("x86_64-unknown-linux-musl", "aarch64-unknown-linux-gnu", TARGET):
                with self.subTest(target=target), self.assertRaises(ValueError):
                    MODULE.require_target(target)

    def test_macos_pinned_make_uses_system_libraries_despite_operator_homebrew(self):
        archive = (SCRIPT.parent / "git-runtime/git-2.56.0.tar.xz").read_bytes()
        prefix = self.root / "operator-homebrew"
        for package in ("gettext", "libiconv"):
            (prefix / "opt" / package / "include").mkdir(parents=True)
            (prefix / "opt" / package / "lib").mkdir()
        for target, machine in (("x86_64-apple-darwin", "x86_64"), (TARGET, "arm64")):
            with self.subTest(target=target):
                directory = self.root / target
                directory.mkdir()
                source = MODULE.extract_source(archive, directory)
                probe = source / "bootstrap-query.mk"
                probe.write_text(
                    "bootstrap-print-libraries:\n"
                    "\t@printf 'ICONVDIR=%s\\nCURL_CONFIG=%s\\nCFLAGS=%s\\nLDFLAGS=%s\\nLIBS=%s\\n' "
                    "'$(ICONVDIR)' '$(CURL_CONFIG)' '$(BASIC_CFLAGS)' '$(BASIC_LDFLAGS)' '$(EXTLIBS)'\n"
                )
                arguments = MODULE.make_arguments(target, 1, {})[:-2]
                options = ["-s", "-f", "Makefile", "-f", str(probe), "uname_S=Darwin", "uname_R=24.0.0", f"uname_M={machine}", f"HOMEBREW_PREFIX={prefix}", "NO_CURL=YesPlease", "bootstrap-print-libraries"]
                # Show the exact upstream route that the publisher must block,
                # without compiling or relying on a host package-manager path.
                unforced = [arg for arg in arguments if not arg.startswith(("NO_HOMEBREW=", "NO_FINK=", "NO_DARWIN_PORTS=", "ICONVDIR=", "CURL_CONFIG="))]
                old = subprocess.run(unforced + options, cwd=source, capture_output=True, text=True, timeout=30, check=True).stdout
                self.assertIn(f"ICONVDIR={prefix}/opt/libiconv", old)
                self.assertIn(f"-L{prefix}/opt/gettext/lib", old)
                selected = subprocess.run(arguments + options, cwd=source, capture_output=True, text=True, timeout=30, check=True).stdout
                self.assertIn("ICONVDIR=/usr\n", selected)
                self.assertIn("CURL_CONFIG=/usr/bin/curl-config\n", selected)
                self.assertIn("-liconv", selected)
                self.assertNotIn(str(prefix), selected)
                self.assertNotIn("-I/sw/", selected)
                self.assertNotIn("-L/opt/local/", selected)

    def test_macos_toolchain_identity_binds_the_selected_system_curl(self):
        for target in ("x86_64-apple-darwin", TARGET):
            with self.subTest(target=target), patch.object(MODULE.subprocess, "check_output", return_value=b"fixed toolchain") as probe:
                before = MODULE.toolchain_identity({"PATH": "/operator/homebrew"}, target)
                curl_commands = [call.args[0] for call in probe.call_args_list if "curl-config" in call.args[0][0]]
                self.assertTrue(curl_commands)
                self.assertTrue(all(command[0] == "/usr/bin/curl-config" for command in curl_commands))
                program = Path(self.programs["/usr/bin/curl-config"])
                program.write_bytes(program.read_bytes() + b"updated system curl")
                self.assertNotEqual(before, MODULE.toolchain_identity({"PATH": "/operator/homebrew"}, target))

    def configure_build(self):
        scripts = self.root / "scripts/git-runtime"
        scripts.mkdir(parents=True)
        policy = scripts / "policy.patch"
        policy.write_bytes(b"fixed fixture patch")
        source = self.root / "source.tar.xz"
        source.write_bytes(archive_bytes([("git-2.56.0/source.c", b"fixture", "file")]))
        self.enterContext(patch.object(MODULE, "ROOT", self.root))
        self.enterContext(patch.object(MODULE, "SOURCE_SHA256", MODULE.digest(source.read_bytes())))
        self.enterContext(patch.object(MODULE, "PATCH_SHA256", MODULE.digest(policy.read_bytes())))
        self.enterContext(patch.object(MODULE, "native_target", return_value=TARGET))
        producer_spec = importlib.util.spec_from_file_location("fixture_producer", SCRIPT.parent / "prepare-git-runtime.py")
        producer = importlib.util.module_from_spec(producer_spec)
        producer_spec.loader.exec_module(producer)
        self.enterContext(patch.object(producer, "SOURCE_SHA256", MODULE.SOURCE_SHA256))
        self.enterContext(patch.object(MODULE, "prepare_module", return_value=producer))
        self.enterContext(patch.object(MODULE.subprocess, "check_output", return_value="binary:\n\t/usr/lib/libSystem.B.dylib (compatibility version 1)\n"))
        return dict(target=TARGET, source_archive=source, policy_patch=policy, cache_root=self.root / "cache", jobs=1)

    def fixture_build(self, command, cwd, env, *, timeout=900):
        self.assertEqual(env.get("HOME"), os.environ.get("HOME"))
        self.assertFalse(any(key.startswith("GIT_") for key in env))
        self.assertNotIn("MINI_AGENT_GIT_GLOBAL_SNAPSHOT", env)
        for variable in ("MAKEFLAGS", "MFLAGS", "PROFILE"):
            self.assertNotIn(variable, env)
        if command[0] == "make":
            for flag in ("NO_RUST=YesPlease", "NO_GETTEXT=YesPlease", "NO_TCLTK=YesPlease", "NO_PERL=YesPlease", "NO_PYTHON=YesPlease"):
                self.assertIn(flag, command)
            for name in ("git", "git-remote-http"):
                (cwd / name).write_bytes(macho())

    def test_build_pins_inputs_cleans_sources_and_reuses_verified_cache(self):
        args = self.configure_build()
        with patch.dict(os.environ, {"GIT_EXEC_PATH": "/invalid", "GIT_REPO_CONFIG_POLICY": "invalid", "MINI_AGENT_GIT_GLOBAL_SNAPSHOT": "invalid", "PROFILE": "debug", "MAKEFLAGS": "invalid", "MFLAGS": "invalid"}), patch.object(MODULE, "run", side_effect=self.fixture_build) as run:
            first = MODULE.bootstrap(output=self.root / "bundle-one", **args)
            calls = run.call_count
            second = MODULE.bootstrap(output=self.root / "bundle-two", **args)
            self.assertEqual(run.call_count, calls)
        MODULE.validate_bundle(first, TARGET)
        MODULE.validate_bundle(second, TARGET)
        self.assertEqual((first / "bundle.manifest").read_bytes(), (second / "bundle.manifest").read_bytes())
        self.assertFalse(list(args["cache_root"].rglob("source.c")))
        self.assertFalse(list(args["cache_root"].rglob(".building-*")))
        cached_git = next(args["cache_root"].glob("*/*/bundle/git"))
        cached_git.write_bytes(b"modified")
        with patch.object(MODULE, "run") as run, self.assertRaisesRegex(ValueError, "hash mismatch"):
            MODULE.bootstrap(output=self.root / "not-created", **args)
        run.assert_not_called()
        self.assertFalse((self.root / "not-created").exists())

    def test_static_library_byte_changes_invalidate_cached_toolchain(self):
        for name in ("curl", "c", "gcc", "gcc_eh"):
            (self.root / ("lib" + name + ".a")).write_bytes(name.encode())
        def probe(command, **kwargs):
            if command[0] == "pkg-config":
                return "-L" + str(self.root) + " -lcurl"
            if command[0] == "cc" and command[1].startswith("--print-file-name="):
                return str(self.root / command[1].split("=", 1)[1])
            return b"fixed compiler and backend version"
        with patch.object(MODULE.subprocess, "check_output", side_effect=probe):
            first = MODULE.toolchain_identity({}, "aarch64-unknown-linux-musl")
            self.assertEqual(first, MODULE.toolchain_identity({}, "aarch64-unknown-linux-musl"))
            (self.root / "libcurl.a").write_bytes(b"security patch under unchanged version")
            self.assertNotEqual(first, MODULE.toolchain_identity({}, "aarch64-unknown-linux-musl"))

    def test_macos_non_system_dylib_still_refuses_publication_and_cache(self):
        args = self.configure_build()
        def probe(command, **_kwargs):
            if command[0] == "otool":
                return "binary:\n\t/usr/lib/libSystem.B.dylib (compatibility version 1)\n\t/operator/homebrew/lib/libiconv.2.dylib (compatibility version 1)\n"
            return b"fixed toolchain"
        with patch.object(MODULE.subprocess, "check_output", side_effect=probe), patch.object(MODULE, "run", side_effect=self.fixture_build):
            with self.assertRaisesRegex(ValueError, "non-system dynamic library dependency"):
                MODULE.bootstrap(output=self.root / "not-published", **args)
        self.assertFalse((self.root / "not-published").exists())
        self.assertFalse(list(args["cache_root"].rglob("bundle.manifest")))
        self.assertFalse(list(args["cache_root"].rglob(".building-*")))

    def test_missing_native_toolchain_program_is_named_before_build(self):
        for missing in ("cc", "curl-config"):
            with self.subTest(program=missing), patch.object(
                MODULE.shutil, "which",
                side_effect=lambda program, **_kwargs: None if program == missing else sys.executable,
            ), patch.object(MODULE, "read_regular", return_value=b"compiler"), patch.object(
                MODULE.subprocess, "check_output"
            ) as command:
                with self.assertRaisesRegex(ValueError, "unavailable: " + missing + r"\. .*(build-essential|libcurl4-openssl-dev)"):
                    MODULE.toolchain_identity({}, "x86_64-unknown-linux-gnu")
                command.assert_not_called()

    def test_changed_tls_toolchain_cannot_reuse_previous_payload(self):
        args = self.configure_build()
        with patch.object(MODULE, "toolchain_identity", side_effect=["openssl", "mbedtls"]), patch.object(MODULE, "run", side_effect=self.fixture_build) as run:
            MODULE.bootstrap(output=self.root / "tls-one", **args)
            first_calls = run.call_count
            MODULE.bootstrap(output=self.root / "tls-two", **args)
            self.assertGreater(run.call_count, first_calls)
        self.assertEqual(len(list(args["cache_root"].glob("*/*/bundle/git"))), 2)

    def test_compile_budget_only_changes_make_deadline_and_native_default_is_900(self):
        args = self.configure_build()
        for budget in (None, 1700):
            with self.subTest(budget=budget):
                parameters = dict(args, cache_root=self.root / f"cache-{budget}")
                if budget is not None:
                    parameters["compile_timeout_seconds"] = budget
                with patch.object(MODULE, "run", side_effect=self.fixture_build) as execute:
                    result = MODULE.bootstrap(output=self.root / f"bundle-{budget}", **parameters)
                self.assertTrue((result / "bundle.manifest").is_file())
                deadlines = {call.args[0][0]: call.kwargs.get("timeout", 900) for call in execute.call_args_list}
                self.assertEqual(deadlines["make"], 900 if budget is None else budget)
                self.assertEqual(deadlines["patch"], 900)
                self.assertEqual(deadlines[next(program for program in deadlines if program.endswith("/git"))], 900)

    def test_compile_budget_cli_overrides_environment_and_invalid_settings_never_build(self):
        base = [str(SCRIPT), "--target", TARGET, "--output", str(self.root / "cli-output")]
        for environment, command_line, expected in ((None, [], 900), ("", [], 900), ("1200", [], 1200), ("1200", ["--compile-timeout-seconds", "1700"], 1700), ("invalid", ["--compile-timeout-seconds", "1700"], 1700)):
            with self.subTest(environment=environment, command_line=command_line):
                settings = dict(os.environ)
                settings.pop(MODULE.COMPILE_TIMEOUT_ENV, None)
                if environment is not None:
                    settings[MODULE.COMPILE_TIMEOUT_ENV] = environment
                with patch.dict(os.environ, settings, clear=True), patch.object(sys, "argv", base + command_line), patch.object(MODULE.signal, "signal"), patch.object(MODULE, "bootstrap", return_value=self.root) as build, patch.object(sys, "stdout", io.StringIO()):
                    MODULE.main()
                self.assertEqual(build.call_args.kwargs["compile_timeout_seconds"], expected)
        for value in ("0", "-1", "86401", "1.5", "nan"):
            for command_line in (False, True):
                with self.subTest(value=value, command_line=command_line), patch.dict(os.environ, {MODULE.COMPILE_TIMEOUT_ENV: value}), patch.object(sys, "argv", base + (["--compile-timeout-seconds", value] if command_line else [])), patch.object(MODULE, "bootstrap") as build, patch.object(sys, "stderr", io.StringIO()), self.assertRaises(SystemExit) as refused:
                    MODULE.main()
                self.assertEqual(refused.exception.code, 2)
                build.assert_not_called()
        args = self.configure_build()
        for value in (0, -1, 86401, True, 1.5):
            with patch.object(MODULE, "run") as execute, self.assertRaises(ValueError):
                MODULE.bootstrap(output=self.root / "invalid-budget", compile_timeout_seconds=value, **args)
            execute.assert_not_called()

    def test_wrong_archive_or_patch_never_starts_build(self):
        args = self.configure_build()
        for field in ("source_archive", "policy_patch"):
            original = args[field].read_bytes()
            args[field].write_bytes(b"wrong input")
            with patch.object(MODULE, "run") as run, self.assertRaisesRegex(ValueError, "pinned hash"):
                MODULE.bootstrap(output=self.root / "out", **args)
            run.assert_not_called()
            args[field].write_bytes(original)
        self.assertFalse(args["cache_root"].exists())

    def test_failed_build_removes_only_its_owned_staging(self):
        args = self.configure_build()
        unrelated = self.root / "unrelated"
        unrelated.write_bytes(b"keep")
        with patch.object(MODULE, "run", side_effect=subprocess.CalledProcessError(2, "make")), self.assertRaises(subprocess.CalledProcessError):
            MODULE.bootstrap(output=self.root / "out", **args)
        self.assertEqual(unrelated.read_bytes(), b"keep")
        self.assertFalse(list(args["cache_root"].rglob(".building-*")))
        self.assertFalse((self.root / "out").exists())

    def test_existing_output_and_insecure_cache_are_not_repaired(self):
        args = self.configure_build()
        out = self.root / "out"
        out.mkdir()
        (out / "keep").write_bytes(b"keep")
        with self.assertRaisesRegex(ValueError, "already exist"):
            MODULE.bootstrap(output=out, **args)
        self.assertEqual((out / "keep").read_bytes(), b"keep")
        args["cache_root"].mkdir(mode=0o755)
        args["cache_root"].chmod(0o755)
        with self.assertRaisesRegex(ValueError, "private directory"):
            MODULE.bootstrap(output=self.root / "new", **args)
        self.assertEqual(args["cache_root"].stat().st_mode & 0o777, 0o755)

    def test_make_interruption_reaps_owned_child(self):
        original_popen = subprocess.Popen
        child_holder = []

        def interrupted_popen(*args, **kwargs):
            child = original_popen(*args, **kwargs)
            child_holder.append(child)
            original_wait = child.wait
            first = True

            def interrupted_wait(*args, **kwargs):
                nonlocal first
                if first:
                    first = False
                    raise KeyboardInterrupt
                return original_wait(*args, **kwargs)

            child.wait = interrupted_wait
            return child

        with patch.object(MODULE.subprocess, "Popen", side_effect=interrupted_popen), self.assertRaises(KeyboardInterrupt):
            MODULE.run([sys.executable, "-c", "import time; time.sleep(300)"], self.root, os.environ.copy())
        self.assertIsNotNone(child_holder[0].returncode)
        with self.assertRaises(ProcessLookupError):
            os.kill(child_holder[0].pid, 0)

    def test_make_timeout_kills_and_reaps_the_owned_process_group(self):
        original_popen = subprocess.Popen
        children = []
        def launch(*args, **kwargs):
            child = original_popen(*args, **kwargs)
            children.append(child)
            return child
        with patch.object(MODULE.subprocess, "Popen", side_effect=launch), self.assertRaises(subprocess.TimeoutExpired):
            MODULE.run([sys.executable, "-c", "import time; time.sleep(300)"], self.root, os.environ.copy(), timeout=0.05)
        self.assertIsNotNone(children[0].returncode)
        with self.assertRaises(ProcessLookupError):
            os.killpg(children[0].pid, 0)


if __name__ == "__main__":
    unittest.main()
