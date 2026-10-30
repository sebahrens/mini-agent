import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import ghagent_mcp as g  # noqa: E402

SCRIPT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "ghagent_mcp.py")
TOKEN = "ghp_SUPERSECRETTOKEN123"
GENV = {"PATH": os.environ["PATH"], "GIT_CONFIG_GLOBAL": "/dev/null",
        "GIT_CONFIG_NOSYSTEM": "1", "HOME": "/nonexistent"}


def sh(cwd, *args):
    r = subprocess.run(["git", "-c", "user.name=t", "-c", "user.email=t@t",
                        "-c", "commit.gpgsign=false"] + list(args),
                       cwd=cwd, env=GENV, capture_output=True, text=True)
    assert r.returncode == 0, r.stderr
    return r.stdout.strip()


def commit_file(cwd, path, content="x\n", msg="c"):
    full = os.path.join(cwd, path)
    os.makedirs(os.path.dirname(full), exist_ok=True)
    with open(full, "w") as f:
        f.write(content)
    sh(cwd, "add", "-A")
    sh(cwd, "commit", "-m", msg)
    return sh(cwd, "rev-parse", "HEAD")


class Base(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.bare = os.path.join(self.tmp, "remote.git")
        self.work = os.path.join(self.tmp, "work")
        sh(self.tmp, "init", "--bare", "-b", "main", self.bare)
        sh(self.tmp, "clone", self.bare, self.work)
        sh(self.work, "checkout", "-b", "main")
        self.base_sha = commit_file(self.work, "README", "hi\n")
        sh(self.work, "push", "origin", "main")
        sh(self.work, "remote", "set-url", "origin", "https://github.com/acme/widgets.git")
        sh(self.work, "checkout", "-b", "feature")
        self.tokfile = os.path.join(self.tmp, "token")
        with open(self.tokfile, "w") as f:
            f.write(TOKEN + "\n")
        os.chmod(self.tokfile, 0o600)

    def server(self, **kw):
        base = dict(allow_repos=["acme/widgets"], token_file=self.tokfile,
                    remote_url_override=self.bare, cwd=self.work, poll_interval=0.05)
        base.update(kw)
        return g.Server(g.Config(**base))

    def call(self, srv, name, **args):
        return srv.call_tool(name, args)

    def remote_branches(self):
        r = subprocess.run(["git", "-C", self.bare, "branch", "--format=%(refname:short)"],
                           env=GENV, capture_output=True, text=True)
        return r.stdout.split()


class PushTests(Base):
    def test_fast_forward_push(self):
        commit_file(self.work, "a.txt")
        srv = self.server()
        text, err = self.call(srv, "push_branch")
        self.assertFalse(err, text)
        self.assertIn("feature", self.remote_branches())
        commit_file(self.work, "b.txt")
        text, err = self.call(srv, "push_branch", branch="feature")
        self.assertFalse(err, text)

    def test_protected_branch(self):
        sh(self.work, "checkout", "main")
        text, err = self.call(self.server(), "push_branch")
        self.assertTrue(err)
        self.assertIn("protected", text)

    def test_custom_protected(self):
        text, err = self.call(self.server(protected=["feature"]), "push_branch")
        self.assertTrue(err)
        self.assertIn("protected", text)

    def test_wrong_repo(self):
        sh(self.work, "remote", "set-url", "origin", "git@github.com:evil/other.git")
        text, err = self.call(self.server(), "push_branch")
        self.assertTrue(err)
        self.assertIn("not an allowed", text)

    def test_origin_url_forms(self):
        for u in ("https://github.com/Acme/Widgets", "ssh://git@github.com/acme/widgets.git",
                  "git@github.com:acme/widgets.git"):
            self.assertEqual(g.parse_github_url(u), "acme/widgets")
        self.assertIsNone(g.parse_github_url("https://evil.com/acme/widgets.git"))
        self.assertIsNone(g.parse_github_url("https://github.com.evil.com/acme/widgets.git"))

    def test_detached_head(self):
        commit_file(self.work, "a.txt")
        sh(self.work, "checkout", "--detach")
        text, err = self.call(self.server(), "push_branch")
        self.assertTrue(err)
        self.assertIn("detached", text)

    def test_branch_injection(self):
        commit_file(self.work, "a.txt")
        srv = self.server()
        for bad in ("--force", "a..b", "-d", "feature;rm", "a b", "", "x.lock", "../x"):
            text, err = self.call(srv, "push_branch", branch=bad)
            self.assertTrue(err, bad)
            self.assertTrue(text.startswith("refused"), (bad, text))
        self.assertNotIn("feature", self.remote_branches())

    def test_unknown_argument_refused(self):
        text, err = self.call(self.server(), "push_branch", force=True)
        self.assertTrue(err)
        self.assertIn("unknown argument", text)

    def test_branch_mismatch(self):
        sh(self.work, "branch", "other")
        text, err = self.call(self.server(), "push_branch", branch="other")
        self.assertTrue(err)
        self.assertIn("not the checked-out branch", text)

    def test_workflow_change_refused_then_allowed(self):
        commit_file(self.work, ".github/workflows/ci.yml", "on: push\n")
        text, err = self.call(self.server(), "push_branch")
        self.assertTrue(err)
        self.assertIn(".github/workflows changed", text)
        self.assertNotIn("feature", self.remote_branches())
        text, err = self.call(self.server(allow_workflow_changes=True), "push_branch")
        self.assertFalse(err, text)
        self.assertIn("feature", self.remote_branches())

    def test_workflow_added_then_deleted_still_refused(self):
        commit_file(self.work, ".github/workflows/ci.yml", "on: push\n")
        sh(self.work, "rm", "-q", ".github/workflows/ci.yml")
        sh(self.work, "commit", "-m", "rm")
        text, err = self.call(self.server(), "push_branch")
        self.assertTrue(err)
        self.assertIn(".github/workflows changed", text)

    def test_tree_differs_from_remote_default(self):
        # remote main gains a workflow after our branch point
        other = os.path.join(self.tmp, "other")
        sh(self.tmp, "clone", self.bare, other)
        commit_file(other, ".github/workflows/ci.yml", "on: push\n")
        sh(other, "push", "origin", "HEAD:main")
        commit_file(self.work, "a.txt")
        text, err = self.call(self.server(), "push_branch")
        self.assertTrue(err)
        self.assertIn(".github/workflows changed", text)

    def test_non_fast_forward_rejected_without_force(self):
        commit_file(self.work, "a.txt", "1\n")
        srv = self.server()
        self.assertFalse(self.call(srv, "push_branch")[1])
        remote_sha = sh(self.bare, "rev-parse", "feature")
        sh(self.work, "reset", "-q", "--hard", "HEAD~1")
        commit_file(self.work, "a.txt", "2\n", msg="diverge")
        text, err = self.call(srv, "push_branch")
        self.assertTrue(err)
        self.assertIn("rebase", text)
        self.assertEqual(sh(self.bare, "rev-parse", "feature"), remote_sha)

    def test_token_never_in_output_even_on_failure(self):
        commit_file(self.work, "a.txt")
        # URL embeds the token so git's own error message would leak it.
        bad = "/nonexistent/%s/repo.git" % TOKEN
        srv = self.server(remote_url_override=bad)
        text, err = self.call(srv, "push_branch")
        self.assertTrue(err)
        self.assertNotIn(TOKEN, text)
        self.assertIn("***", text)
        # also the fatal-ref/refusal paths
        text, err = self.call(srv, "wait_for_checks", pr=0)
        self.assertNotIn(TOKEN, text)

    def test_token_file_permissions(self):
        os.chmod(self.tokfile, 0o644)
        text, err = self.call(self.server(), "push_branch")
        self.assertTrue(err)
        self.assertIn("token file", text)
        self.assertNotIn(TOKEN, text)

    def test_git_env_carries_header_not_argv(self):
        env = self.server().git_env(TOKEN)
        self.assertEqual(env["GIT_CONFIG_COUNT"], "1")
        self.assertEqual(env["GIT_CONFIG_KEY_0"], "http.https://github.com/.extraheader")
        self.assertTrue(env["GIT_CONFIG_VALUE_0"].startswith("AUTHORIZATION: basic "))


class FakeGh(Base):
    def setUp(self):
        super().setUp()
        self.bin = os.path.join(self.tmp, "bin")
        self.ghd = os.path.join(self.tmp, "ghd")
        os.makedirs(self.bin)
        os.makedirs(self.ghd)
        script = os.path.join(self.bin, "gh")
        with open(script, "w") as f:
            f.write('#!/bin/sh\nD="%s"\necho "$@" >> "$D/calls.log"\n'
                    'env | grep -q "^GH_TOKEN=" || exit 9\n'
                    '[ "$GH_PROMPT_DISABLED" = 1 ] || exit 9\n'
                    'case "$1" in\n'
                    ' api) case "$*" in *pulls*) cat "$D/pr.json";; *compare*) cat "$D/compare.json" || exit 1;;'
                    ' *check-runs*) if [ -f "$D/fgpat" ]; then'
                    ' echo "gh: Resource not accessible by personal access token (HTTP 403)" >&2; exit 1;'
                    ' fi; cat "$D/checks.json";;'
                    ' */jobs*) cat "$D/jobs.json";; *actions/runs*) cat "$D/wfruns.json";;'
                    ' */status) cat "$D/status.json";; *commits/*) cat "$D/commit.json";;'
                    ' *) exit 1;; esac;;\n'
                    ' release) echo "$@" > "$D/release.args"; cp "${12}" "$D/notes.txt" 2>/dev/null;'
                    ' echo https://github.com/acme/widgets/releases/tag/x;;\n'
                    ' *) exit 1;;\nesac\n' % self.ghd)
        os.chmod(script, 0o755)
        self.set_compare("behind")
        p = mock.patch.dict(os.environ, {"PATH": self.bin + os.pathsep + os.environ["PATH"]})
        p.start()
        self.addCleanup(p.stop)

    def set_compare(self, status):
        with open(os.path.join(self.ghd, "compare.json"), "w") as f:
            json.dump({"status": status}, f)

    def set_checks(self, runs):
        with open(os.path.join(self.ghd, "checks.json"), "w") as f:
            json.dump({"total_count": len(runs), "check_runs": runs}, f)

    def set_fine_grained(self, jobs, statuses=(), sha="b" * 40):
        """Simulate a fine-grained PAT: check-runs 403, Actions + statuses readable."""
        def put(name, obj):
            with open(os.path.join(self.ghd, name), "w") as f:
                json.dump(obj, f)
        open(os.path.join(self.ghd, "fgpat"), "w").close()
        put("commit.json", {"sha": sha})
        put("wfruns.json", {"workflow_runs": [{"id": 11}]})
        put("jobs.json", {"jobs": list(jobs)})
        put("status.json", {"statuses": list(statuses)})

    def calls(self):
        with open(os.path.join(self.ghd, "calls.log")) as f:
            return f.read()


class WaitTests(FakeGh):
    def test_complete_summary(self):
        self.set_checks([{"name": "a", "status": "completed", "conclusion": "success"},
                         {"name": "b", "status": "completed", "conclusion": "failure"}])
        text, err = self.call(self.server(), "wait_for_checks", timeout_s=5)
        self.assertFalse(err, text)
        d = json.loads(text)
        self.assertTrue(d["complete"])
        self.assertEqual(d["counts"], {"success": 1, "failure": 1})
        self.assertEqual(d["failed"], ["b"])
        self.assertNotIn(TOKEN, self.calls())

    def test_timeout_pending(self):
        self.set_checks([{"name": "a", "status": "in_progress", "conclusion": None}])
        text, err = self.call(self.server(), "wait_for_checks", timeout_s=1)
        d = json.loads(text)
        self.assertFalse(d["complete"])
        self.assertEqual(d["pending"], ["a"])

    def test_pr(self):
        with open(os.path.join(self.ghd, "pr.json"), "w") as f:
            json.dump({"head": {"sha": "a" * 40}}, f)
        self.set_checks([{"name": "a", "status": "completed", "conclusion": "success"}])
        text, err = self.call(self.server(), "wait_for_checks", pr=7, timeout_s=3)
        self.assertFalse(err, text)
        self.assertIn("a" * 40, self.calls())

    def test_fine_grained_token_falls_back_to_actions_jobs_and_statuses(self):
        self.set_fine_grained(
            jobs=[{"name": "build", "status": "completed", "conclusion": "success"},
                  {"name": "docs", "status": "completed", "conclusion": "skipped"},
                  {"name": "lint", "status": "completed", "conclusion": "failure"}],
            statuses=[{"context": "ext/ci", "state": "pending"}])
        text, err = self.call(self.server(), "wait_for_checks", ref="main", timeout_s=1)
        self.assertFalse(err, text)
        d = json.loads(text)
        self.assertEqual(d["total"], 4)
        self.assertEqual(d["failed"], ["lint"])
        self.assertEqual(d["pending"], ["ext/ci"])
        self.assertFalse(d["complete"])
        log = self.calls()
        self.assertIn("actions/runs?head_sha=" + "b" * 40, log)
        self.assertIn("actions/runs/11/jobs", log)
        self.assertNotIn(TOKEN, log)

    def test_check_runs_errors_other_than_403_are_not_masked(self):
        os.remove(os.path.join(self.ghd, "checks.json")) if os.path.exists(
            os.path.join(self.ghd, "checks.json")) else None
        text, err = self.call(self.server(), "wait_for_checks", ref="main", timeout_s=1)
        self.assertTrue(err, text)
        self.assertNotIn("actions/runs", self.calls())

    def test_bad_ref_and_timeout_clamped(self):
        text, err = self.call(self.server(), "wait_for_checks", ref="--evil")
        self.assertTrue(err)
        text, err = self.call(self.server(), "wait_for_checks", timeout_s="9")
        self.assertTrue(err)


class ReleaseTests(FakeGh):
    def setUp(self):
        super().setUp()
        self.srv = self.server(allow_releases=True)
        self.ok = [{"name": "a", "status": "completed", "conclusion": "success"}]

    def test_success(self):
        self.set_checks(self.ok)
        text, err = self.call(self.srv, "create_release", tag="v1.2.3",
                              target_sha=self.base_sha, notes="hello")
        self.assertFalse(err, text)
        with open(os.path.join(self.ghd, "release.args")) as f:
            args = f.read()
        self.assertIn("--draft", args)
        self.assertIn("v1.2.3", args)
        self.assertNotIn(TOKEN, args)

    def test_refusals(self):
        self.set_checks(self.ok)
        for kw, frag in [
            (dict(tag="bad", target_sha=self.base_sha), "pattern"),
            (dict(tag="v1.0.0", target_sha="abc"), "40-hex"),
            (dict(tag="v1.0.0", target_sha=self.base_sha, notes="x" * 20001), "20000"),
        ]:
            text, err = self.call(self.srv, "create_release", **kw)
            self.assertTrue(err)
            self.assertIn(frag, text)
        self.set_checks([{"name": "a", "status": "completed", "conclusion": "failure"}])
        text, err = self.call(self.srv, "create_release", tag="v1.0.0", target_sha=self.base_sha)
        self.assertTrue(err)
        self.assertIn("not all successful", text)

    def test_skipped_jobs_allow_release_but_failures_and_pending_block(self):
        ok = {"name": "build", "status": "completed", "conclusion": "success"}
        skip = {"name": "docs", "status": "completed", "conclusion": "skipped"}
        self.set_fine_grained(jobs=[ok, skip])
        text, err = self.call(self.srv, "create_release", tag="v2.0.0", target_sha=self.base_sha)
        self.assertFalse(err, text)
        self.set_fine_grained(jobs=[ok, skip, {"name": "w", "status": "in_progress",
                                                "conclusion": None}])
        text, err = self.call(self.srv, "create_release", tag="v2.0.1", target_sha=self.base_sha)
        self.assertTrue(err)
        self.assertIn("not all successful", text)
        self.set_fine_grained(jobs=[skip])
        text, err = self.call(self.srv, "create_release", tag="v2.0.2", target_sha=self.base_sha)
        self.assertTrue(err, "only skipped jobs must not count as passing")

    def test_unreachable_and_existing_tag(self):
        self.set_checks(self.ok)
        side = commit_file(self.work, "a.txt")  # on feature, not pushed to main
        self.set_compare("ahead")
        text, err = self.call(self.srv, "create_release", tag="v1.0.0", target_sha=side)
        self.assertTrue(err)
        self.assertIn("not reachable", text)
        sh(self.work, "push", self.bare, "%s:refs/tags/v1.0.0" % self.base_sha)
        text, err = self.call(self.srv, "create_release", tag="v1.0.0", target_sha=self.base_sha)
        self.assertTrue(err)
        self.assertIn("already exists", text)


class ProtocolTests(Base):
    def test_tools_list_hides_release(self):
        names = [t["name"] for t in self.server().tools_list()]
        self.assertEqual(names, ["push_branch", "wait_for_checks"])
        names = [t["name"] for t in self.server(allow_releases=True).tools_list()]
        self.assertIn("create_release", names)
        text, err = (None, None)
        with self.assertRaises(KeyError):
            self.server().call_tool("create_release", {})

    def test_framing_over_stdio(self):
        msgs = [
            {"jsonrpc": "2.0", "id": 1, "method": "initialize",
             "params": {"protocolVersion": "2024-11-05"}},
            {"jsonrpc": "2.0", "method": "notifications/initialized"},
            {"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
            {"jsonrpc": "2.0", "id": 3, "method": "ping"},
            {"jsonrpc": "2.0", "id": 4, "method": "nope/nothing"},
            {"jsonrpc": "2.0", "id": 5, "method": "tools/call",
             "params": {"name": "push_branch", "arguments": {"branch": "--force"}}},
        ]
        inp = "\n".join(json.dumps(m) for m in msgs) + "\nnot json\n"
        r = subprocess.run(
            [sys.executable, "-I", SCRIPT, "--allow-repo", "acme/widgets",
             "--token-file", self.tokfile, "--remote-url-override", self.bare],
            input=inp, cwd=self.work, capture_output=True, text=True, timeout=30)
        lines = [json.loads(l) for l in r.stdout.splitlines()]
        self.assertEqual(len(lines), 6)  # no reply to the notification
        self.assertEqual(lines[0]["result"]["protocolVersion"], "2024-11-05")
        self.assertEqual(lines[0]["result"]["capabilities"], {"tools": {}})
        self.assertEqual(lines[0]["result"]["serverInfo"]["name"], "ghagent-mcp")
        self.assertEqual([t["name"] for t in lines[1]["result"]["tools"]],
                         ["push_branch", "wait_for_checks"])
        self.assertEqual(lines[2]["result"], {})
        self.assertEqual(lines[3]["error"]["code"], -32601)
        self.assertTrue(lines[4]["result"]["isError"])
        self.assertEqual(lines[4]["result"]["content"][0]["type"], "text")
        self.assertEqual(lines[5]["error"]["code"], -32700)
        self.assertNotIn(TOKEN, r.stdout + r.stderr)

    def test_requires_allow_repo_and_token(self):
        r = subprocess.run([sys.executable, "-I", SCRIPT, "--token-file", self.tokfile],
                           capture_output=True, text=True, stdin=subprocess.DEVNULL)
        self.assertNotEqual(r.returncode, 0)
        r = subprocess.run([sys.executable, "-I", SCRIPT, "--allow-repo", "a/b"],
                           capture_output=True, text=True, stdin=subprocess.DEVNULL)
        self.assertNotEqual(r.returncode, 0)


# --------------------------------------------------------------------------
# Regression tests for the hardening findings
# --------------------------------------------------------------------------

def alive(pid):
    try:
        with open("/proc/%d/stat" % pid) as f:
            return f.read().rsplit(")", 1)[1].split()[0] != "Z"
    except OSError:
        return False


class HardeningTests(Base):
    def push_feature_remote(self):
        """Remote feature has commit R; returns R."""
        r = commit_file(self.work, "r.txt", "r\n", msg="R")
        sh(self.work, "push", self.bare, "feature")
        return r

    def remote_sha(self, br="feature"):
        return sh(self.bare, "rev-parse", br)

    # 1 -----------------------------------------------------------------
    def _diverge_with_graft(self):
        r = self.push_feature_remote()
        sh(self.work, "reset", "-q", "--hard", self.base_sha)
        l = commit_file(self.work, "l.txt", "l\n", msg="L")
        return r, l

    def test_grafts_cannot_force_update(self):
        r, l = self._diverge_with_graft()
        with open(os.path.join(self.work, ".git", "info", "grafts"), "w") as f:
            f.write("%s %s\n" % (l, r))
        text, err = self.call(self.server(), "push_branch")
        self.assertTrue(err)
        self.assertIn("grafts", text)
        self.assertEqual(self.remote_sha(), r)

    def test_shallow_refused(self):
        r, l = self._diverge_with_graft()
        with open(os.path.join(self.work, ".git", "shallow"), "w") as f:
            f.write(r + "\n")
        text, err = self.call(self.server(), "push_branch")
        self.assertTrue(err)
        self.assertIn("shallow", text)
        self.assertEqual(self.remote_sha(), r)

    def test_grafts_ignored_even_if_presence_check_bypassed(self):
        r, l = self._diverge_with_graft()
        with open(os.path.join(self.work, ".git", "info", "grafts"), "w") as f:
            f.write("%s %s\n" % (l, r))
        srv = self.server()
        with mock.patch.object(g.Server, "workspace_objdir",
                               lambda s_: os.path.join(self.work, ".git", "objects")):
            text, err = self.call(srv, "push_branch")
        self.assertTrue(err)
        self.assertIn("rebase", text)
        self.assertEqual(self.remote_sha(), r)

    # 2 -----------------------------------------------------------------
    def test_replace_ref_cannot_hide_workflow(self):
        parent_tree = sh(self.work, "rev-parse", "HEAD^{tree}")
        t = commit_file(self.work, ".github/workflows/ci.yml", "on: push\n")
        c = sh(self.work, "commit-tree", parent_tree, "-p", self.base_sha, "-m", "innocent")
        sh(self.work, "replace", t, c)
        text, err = self.call(self.server(), "push_branch")
        self.assertTrue(err, text)
        self.assertIn(".github/workflows changed", text)
        self.assertNotIn("feature", self.remote_branches())

    # 3 -----------------------------------------------------------------
    def test_workspace_base_ref_irrelevant(self):
        commit_file(self.work, ".github/workflows/ci.yml", "on: push\n")
        sh(self.work, "update-ref", "refs/ghagent/base", "HEAD")
        text, err = self.call(self.server(), "push_branch")
        self.assertTrue(err)
        self.assertIn(".github/workflows changed", text)
        self.assertNotIn("feature", self.remote_branches())

    def test_tip_swap_after_resolution_does_not_change_push(self):
        tip = commit_file(self.work, "a.txt")
        orig = g.Server.fetch_remote

        def swap(self_, *a, **k):
            commit_file(self.work, ".github/workflows/evil.yml", "on: push\n")
            return orig(self_, *a, **k)
        with mock.patch.object(g.Server, "fetch_remote", swap):
            text, err = self.call(self.server(), "push_branch")
        self.assertFalse(err, text)
        self.assertEqual(self.remote_sha(), tip)
        self.assertNotEqual(sh(self.work, "rev-parse", "HEAD"), tip)

    # 5 -----------------------------------------------------------------
    def test_relative_and_empty_path_entries_ignored(self):
        marker = os.path.join(self.tmp, "pwned")
        with open(os.path.join(self.work, "git"), "w") as f:
            f.write('#!/bin/sh\ntouch "%s"\nexit 1\n' % marker)
        os.chmod(os.path.join(self.work, "git"), 0o755)
        old = os.getcwd()
        os.chdir(self.work)
        self.addCleanup(os.chdir, old)
        commit_file(self.work, "a.txt")
        with mock.patch.dict(os.environ, {"PATH": ":.::" + os.environ["PATH"]}):
            srv = self.server()
            self.assertTrue(os.path.isabs(srv.git_path))
            self.assertNotEqual(os.path.realpath(srv.git_path),
                                os.path.realpath(os.path.join(self.work, "git")))
            self.assertNotIn("", srv._base_env()["PATH"].split(os.pathsep))
            text, err = self.call(srv, "push_branch")
        self.assertFalse(err, text)
        self.assertFalse(os.path.exists(marker))

    def test_refuses_to_start_without_git(self):
        with mock.patch.object(g.shutil, "which", lambda *a, **k: None):
            with self.assertRaises(g.StartupError):
                self.server()
        with mock.patch.object(g.shutil, "which", lambda *a, **k: "git"):  # relative result
            with self.assertRaises(g.StartupError):
                self.server()
        with mock.patch.object(g, "main", wraps=g.main):
            with mock.patch.object(g.shutil, "which", lambda *a, **k: None):
                self.assertEqual(g.main(["--allow-repo", "a/b", "--token-env", "X"]), 2)

    # 6 -----------------------------------------------------------------
    def test_github_symlink_replacement_refused(self):
        commit_file(self.work, ".github/workflows/ci.yml", "on: push\n")
        sh(self.work, "push", self.bare, "HEAD:refs/heads/main")  # base has real workflow
        sh(self.work, "rm", "-rq", ".github")
        os.symlink("../elsewhere", os.path.join(self.work, ".github"))
        sh(self.work, "add", "-A")
        sh(self.work, "commit", "-m", "symlink")
        text, err = self.call(self.server(), "push_branch")
        self.assertTrue(err)
        self.assertIn(".github/workflows changed", text)

    def test_preexisting_github_symlink_in_tip_refused(self):
        os.symlink("../elsewhere", os.path.join(self.work, ".github"))
        sh(self.work, "add", "-A")
        sh(self.work, "commit", "-m", "symlink")
        sh(self.work, "push", self.bare, "HEAD:refs/heads/main")  # in the base too: no diff
        commit_file(self.work, "a.txt")
        text, err = self.call(self.server(), "push_branch")
        self.assertTrue(err)
        self.assertIn(".github/workflows changed", text)
        self.assertNotIn("feature", self.remote_branches())

    def test_github_actions_and_case_variants_refused(self):
        for path in (".github/actions/x/action.yml", ".GitHub/workflows/ci.yml",
                     ".github/CODEOWNERS"):
            sh(self.work, "checkout", "-q", "-B", "feature", self.base_sha)
            commit_file(self.work, path, "x\n")
            text, err = self.call(self.server(), "push_branch")
            self.assertTrue(err, path)
            self.assertIn(".github/workflows changed", text)
        self.assertNotIn("feature", self.remote_branches())

    # 7 -----------------------------------------------------------------
    def test_trailing_newline_rejected(self):
        commit_file(self.work, "a.txt")
        srv = self.server()
        text, err = self.call(srv, "push_branch", branch="feature\n")
        self.assertTrue(err)
        self.assertTrue(text.startswith("refused"), text)
        self.assertNotIn("feature", self.remote_branches())
        self.assertIsNone(g.parse_github_url("https://github.com/acme/widgets\n.git"))
        with self.assertRaises(SystemExit):
            with mock.patch("sys.stderr"):
                g.parse_args(["--allow-repo", "a/b\n", "--token-env", "X"])
        with self.assertRaises(g.Refusal):
            g.valid_ref_name("v1\n")
        self.assertIsNone(g.SHA_RE.fullmatch("a" * 40 + "\n"))

    # 8 -----------------------------------------------------------------
    def test_timeout_kills_process_group(self):
        srv = self.server()
        pidfile = os.path.join(self.tmp, "pid")
        rc, _, err = srv._run(["/bin/sh", "-c", 'sleep 60 & echo $! > "%s"; wait' % pidfile],
                              srv.git_env(), 1)
        self.assertEqual(rc, 124)
        with open(pidfile) as f:
            pid = int(f.read())
        time.sleep(0.2)
        self.assertFalse(alive(pid))

    def test_overall_deadline_caps_steps(self):
        srv = self.server()
        srv.begin_call(1.0)
        t0 = time.monotonic()
        rc, _, _ = srv._run(["/bin/sleep", "30"], srv.git_env(), 30)
        self.assertEqual(rc, 124)
        self.assertLess(time.monotonic() - t0, 5)
        rc, _, err = srv._run(["/bin/sleep", "30"], srv.git_env(), 30)  # nothing left
        self.assertEqual(rc, 124)
        self.assertIn("deadline", err)

    # 9 -----------------------------------------------------------------
    def test_protected_adds_to_defaults(self):
        cfg = g.Config(["a/b"], protected=["develop"], token_env="X")
        self.assertEqual(cfg.protected, ["main", "master", "develop"])
        a = g.parse_args(["--allow-repo", "a/b", "--token-env", "X", "--protected", "develop"])
        self.assertEqual(a.protected, ["main", "master", "develop"])
        sh(self.work, "checkout", "main")
        text, err = self.call(self.server(protected=["feature"]), "push_branch")
        self.assertTrue(err)
        self.assertIn("protected", text)

    # extra: lease / atomicity ------------------------------------------
    def test_remote_moved_after_fetch_is_not_overwritten(self):
        r = self.push_feature_remote()
        commit_file(self.work, "b.txt")
        other = os.path.join(self.tmp, "other")
        sh(self.tmp, "clone", "-b", "feature", self.bare, other)
        moved = commit_file(other, "o.txt", "o\n", msg="other")
        orig = g.Server.fetch_remote
        calls = []

        def late(self_, *a, **k):
            res = orig(self_, *a, **k)
            if not calls:
                calls.append(1)
                sh(other, "push", "origin", "feature")  # remote advances after our fetch
            return res
        with mock.patch.object(g.Server, "fetch_remote", late):
            text, err = self.call(self.server(), "push_branch")
        self.assertTrue(err)
        self.assertEqual(self.remote_sha(), moved)


if __name__ == "__main__":
    unittest.main()
