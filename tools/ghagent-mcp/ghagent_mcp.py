#!/usr/bin/env python3
"""ghagent-mcp: a small, security-focused stdio MCP server (stdlib only).

Runs OUTSIDE the agent sandbox as trusted code. Its own input validation is the
safety boundary. Configuration comes only from argv. See README.md.
"""
import argparse
import base64
import json
import os
import re
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time
import urllib.parse

SERVER_NAME = "ghagent-mcp"
SERVER_VERSION = "0.2.0"
DEFAULT_PROTOCOL = "2025-06-18"

# All validation uses fullmatch(): "$" would accept a trailing newline.
BRANCH_RE = re.compile(r"[A-Za-z0-9._/-]+")
SHA_RE = re.compile(r"[0-9a-f]{40}")
OWNER_REPO_RE = re.compile(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+")
DEFAULT_PROTECTED = ("main", "master")
CALL_DEADLINE = 100.0  # seconds, overall per tool call (MCP limit is 120 s)
GIT_HARDENING = ["-c", "core.useReplaceRefs=false", "-c", "core.hooksPath=/dev/null",
                 "-c", "core.fsmonitor=false", "-c", "credential.helper=",
                 "-c", "protocol.ext.allow=never", "-c", "protocol.file.allow=user"]
EMPTY_TREE = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
OUT_CAP = 4000
MAX_COMMITS = 500
FAIL_CONCLUSIONS = {"failure", "cancelled", "timed_out", "action_required",
                    "startup_failure", "stale"}


class StartupError(Exception):
    """Server cannot start safely (e.g. git not found)."""


class Refusal(Exception):
    """A policy refusal or tool failure; message is one line, safe to show."""


def log(msg):
    print("ghagent-mcp: " + str(msg), file=sys.stderr, flush=True)


# --------------------------------------------------------------------------
# Pure helpers
# --------------------------------------------------------------------------

def parse_github_url(url):
    """Return 'owner/repo' (lowercased) for a GitHub https/ssh/scp URL, else None."""
    url = (url or "").strip()
    pats = [
        r"^https://(?:[^@/]+@)?github\.com/([^/]+)/([^/]+?)(?:\.git)?/?$",
        r"^ssh://(?:[^@/]+@)?github\.com(?::22)?/([^/]+)/([^/]+?)(?:\.git)?/?$",
        r"^(?:[^@/:]+@)?github\.com:([^/]+)/([^/]+?)(?:\.git)?/?$",
    ]
    for p in pats:
        m = re.match(p, url, re.I)
        if m:
            r = m.group(1) + "/" + m.group(2)
            return r.lower() if OWNER_REPO_RE.fullmatch(r) else None
    return None


def valid_ref_name(name, what="name"):
    if not isinstance(name, str) or not name:
        raise Refusal("refused: %s must be a non-empty string" % what)
    if len(name) > 200 or not BRANCH_RE.fullmatch(name):
        raise Refusal("refused: %s has characters outside [A-Za-z0-9._/-]" % what)
    if name.startswith("-") or ".." in name or name.startswith("/") \
            or name.endswith("/") or name.endswith(".lock") or "//" in name:
        raise Refusal("refused: %s is not a safe ref name" % what)
    return name


def redact(text, token):
    if not text:
        return ""
    if token:
        b64 = base64.b64encode(("x-access-token:" + token).encode()).decode()
        for secret in (b64, token):
            text = text.replace(secret, "***")
    return text


def clip(text, n=OUT_CAP):
    text = text.strip()
    return text if len(text) <= n else text[:n] + "...[truncated]"


# --------------------------------------------------------------------------
# Server
# --------------------------------------------------------------------------

class Config:
    def __init__(self, allow_repos, protected=(),
                 allow_workflow_changes=False, allow_releases=False,
                 tag_pattern=r"v[0-9]+\.[0-9]+\.[0-9]+", token_file=None,
                 token_env=None, remote_url_override=None, poll_interval=10.0,
                 cwd=None):
        self.allow_repos = [r.lower() for r in allow_repos]
        # --protected ADDS to the defaults; main/master can never be dropped.
        self.protected = list(DEFAULT_PROTECTED) + [
            p for p in protected if p not in DEFAULT_PROTECTED]
        self.allow_workflow_changes = allow_workflow_changes
        self.allow_releases = allow_releases
        self.tag_re = re.compile(tag_pattern)
        self.token_file = token_file
        self.token_env = token_env
        self.remote_url_override = remote_url_override
        self.poll_interval = poll_interval
        self.cwd = cwd or os.getcwd()


class Sandbox:
    """A private, empty bare repo for one tool call. All object-graph decisions
    (fetch, ancestry, diff, push) are made here, never in the model-writable
    workspace repo. The workspace object store is attached read-only-by-use via
    GIT_ALTERNATE_OBJECT_DIRECTORIES; objects are content-addressed, so the
    workspace cannot lie about them, and the sandbox has no grafts, shallow
    file, replace refs, config or hooks."""

    def __init__(self, srv, objdir=None):
        self.srv = srv
        self.path = tempfile.mkdtemp(prefix="ghagent-repo-")
        try:
            os.chmod(self.path, 0o700)
            rc, _, err = srv._run(
                [srv.git_path, "init", "--bare", "-q", "--template=", self.path],
                srv.git_env(), 20, cwd=self.path)
            if rc != 0:
                raise Refusal("error: cannot create temporary repository: " + clip(err, 200))
        except BaseException:
            shutil.rmtree(self.path, ignore_errors=True)
            raise
        self.objdir = objdir

    def env(self, token=None):
        env = self.srv.git_env(token)
        env["GIT_DIR"] = self.path
        env["GIT_NO_REPLACE_OBJECTS"] = "1"
        if self.objdir:
            env["GIT_ALTERNATE_OBJECT_DIRECTORIES"] = self.objdir
        return env

    def git(self, args, token=None, timeout=30):
        argv = [self.srv.git_path] + GIT_HARDENING + list(args)
        return self.srv._run(argv, self.env(token), timeout, token, cwd=self.path)

    def close(self):
        shutil.rmtree(self.path, ignore_errors=True)


def safe_path_entries(path):
    """PATH entries that are non-empty and absolute (relative/empty entries
    would resolve inside the agent-controlled cwd)."""
    return [e for e in (path or "").split(os.pathsep) if e and os.path.isabs(e)]


class Server:
    def __init__(self, cfg):
        self.cfg = cfg
        self.safe_path = os.pathsep.join(safe_path_entries(os.environ.get("PATH"))) \
            or "/usr/bin:/bin"
        self.git_path = shutil.which("git", path=self.safe_path)
        if not self.git_path or not os.path.isabs(self.git_path):
            raise StartupError("git not found on the absolute entries of PATH")
        gh = shutil.which("gh", path=self.safe_path)
        self.gh_path = gh if gh and os.path.isabs(gh) else None
        self._deadline = None

    # ---- deadline -------------------------------------------------------
    def begin_call(self, seconds=CALL_DEADLINE):
        self._deadline = time.monotonic() + seconds

    def _remaining(self):
        return None if self._deadline is None else self._deadline - time.monotonic()

    # ---- token / env ----------------------------------------------------
    def load_token(self):
        c = self.cfg
        if c.token_file:
            try:
                st = os.stat(c.token_file)
                if not stat.S_ISREG(st.st_mode):
                    raise Refusal("refused: token file is not a regular file")
                if st.st_mode & 0o077:
                    raise Refusal("refused: token file is group/world accessible (chmod 600)")
                with open(c.token_file, "r") as f:
                    tok = f.read().strip()
            except OSError:
                raise Refusal("refused: cannot read token file")
        else:
            tok = (os.environ.get(c.token_env or "", "")).strip()
        if not tok or re.search(r"\s", tok):
            raise Refusal("refused: no usable token configured")
        return tok

    def _base_env(self):
        return {"PATH": self.safe_path, "LC_ALL": "C", "LANG": "C"}

    def git_env(self, token=None):
        env = self._base_env()
        env.update({"GIT_TERMINAL_PROMPT": "0", "GIT_CONFIG_NOSYSTEM": "1",
                    "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_ASKPASS": "true",
                    "HOME": "/nonexistent"})
        if token:
            b64 = base64.b64encode(("x-access-token:" + token).encode()).decode()
            env.update({
                "GIT_CONFIG_COUNT": "1",
                "GIT_CONFIG_KEY_0": "http.https://github.com/.extraheader",
                "GIT_CONFIG_VALUE_0": "AUTHORIZATION: basic " + b64,
            })
        return env

    @staticmethod
    def _killpg(p):
        try:
            os.killpg(p.pid, signal.SIGKILL)
        except (ProcessLookupError, PermissionError, OSError):
            pass

    def _run(self, argv, env, timeout, token=None, cwd=None):
        t = float(timeout)
        rem = self._remaining()
        if rem is not None:
            if rem <= 0:
                return 124, "", "overall call deadline exceeded"
            t = min(t, rem)
        try:
            p = subprocess.Popen(argv, env=env, cwd=cwd, stdin=subprocess.DEVNULL,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                 start_new_session=True)
        except OSError as e:
            return 127, "", "cannot execute %s: %s" % (argv[0], e.strerror)
        try:
            so, se = p.communicate(timeout=t)
        except subprocess.TimeoutExpired:
            # Kill the whole process group (git-remote-https, pack-objects, ...).
            self._killpg(p)
            try:
                p.communicate(timeout=5)
            except Exception:
                pass
            return 124, "", "timed out after %ss" % round(t, 1)
        out = redact(so.decode("utf-8", "replace"), token)
        err = redact(se.decode("utf-8", "replace"), token)
        return p.returncode, out, err

    def wgit(self, args, timeout=30):
        """git in the (untrusted) workspace. Only used for a few narrow reads;
        replace objects are disabled."""
        argv = [self.git_path, "-C", self.cfg.cwd, "--no-replace-objects"] \
            + GIT_HARDENING + list(args)
        env = self.git_env()
        env["GIT_NO_REPLACE_OBJECTS"] = "1"
        return self._run(argv, env, timeout)

    # ---- workspace reads --------------------------------------------------
    def check_origin(self):
        rc, out, _ = self.wgit(["config", "--get", "remote.origin.url"])
        if rc != 0 or not out.strip():
            raise Refusal("refused: workspace has no 'origin' remote")
        repo = parse_github_url(out.strip())
        if repo is None or repo not in self.cfg.allow_repos:
            raise Refusal("refused: origin is not an allowed repository")
        return repo

    def remote_url(self, repo):
        return self.cfg.remote_url_override or \
            "https://github.com/%s.git" % repo

    def current_branch(self):
        rc, out, _ = self.wgit(["symbolic-ref", "-q", "--short", "HEAD"])
        if rc != 0 or not out.strip():
            raise Refusal("refused: detached HEAD; check out a branch first")
        return out.strip()

    def workspace_objdir(self):
        """Absolute workspace object dir (verified inside the workspace); also
        refuse graft/shallow files as defense in depth."""
        ws = os.path.realpath(self.cfg.cwd)
        dirs = []
        for flag in ("--git-common-dir", "--absolute-git-dir"):
            rc, out, _ = self.wgit(["rev-parse", flag])
            if rc != 0 or not out.strip():
                raise Refusal("error: cannot locate the workspace git directory")
            dirs.append(os.path.realpath(os.path.join(ws, out.strip())))
        for d in dirs:
            for rel in ("info/grafts", "shallow"):
                if os.path.lexists(os.path.join(d, rel)):
                    raise Refusal("refused: workspace has .git/%s (history overrides "
                                  "are not allowed); remove it" % rel)
        objdir = os.path.realpath(os.path.join(dirs[0], "objects"))
        try:
            inside = os.path.commonpath([ws, objdir]) == ws
        except ValueError:
            inside = False
        if not os.path.isdir(objdir) or not inside or os.pathsep in objdir:
            raise Refusal("refused: workspace object directory is not a directory "
                          "inside the workspace")
        return objdir

    def resolve_tip(self, branch):
        rc, out, _ = self.wgit(["rev-parse", "--verify", "--quiet",
                                "refs/heads/%s^{commit}" % branch])
        sha = out.strip()
        if rc != 0 or not SHA_RE.fullmatch(sha):
            raise Refusal("refused: cannot resolve the branch tip")
        return sha

    # ---- sandbox (temp repo) operations ------------------------------------
    def discover_remote(self, sb, url, token, branch):
        rc, out, err = sb.git(["ls-remote", "--symref", url, "HEAD",
                               "refs/heads/" + branch], token, timeout=40)
        if rc != 0:
            raise Refusal("error: cannot reach remote: " + clip(err, 300))
        m = re.search(r"^ref: refs/heads/(\S+)\tHEAD$", out, re.M)
        if not m:
            raise Refusal("refused: cannot determine remote default branch")
        default = valid_ref_name(m.group(1), "remote default branch")
        exists = False
        for line in out.splitlines():
            parts = line.split("\t")
            if len(parts) == 2 and parts[1] == "refs/heads/" + branch \
                    and SHA_RE.fullmatch(parts[0]):
                exists = True
        return default, exists

    def remote_default_branch(self, sb, url, token):
        rc, out, err = sb.git(["ls-remote", "--symref", url, "HEAD"], token, timeout=40)
        if rc != 0:
            raise Refusal("error: cannot reach remote: " + clip(err, 300))
        m = re.search(r"^ref: refs/heads/(\S+)\tHEAD$", out, re.M)
        if not m:
            raise Refusal("refused: cannot determine remote default branch")
        return valid_ref_name(m.group(1), "remote default branch")

    def fetch_remote(self, sb, url, token, default, branch, remote_exists):
        specs = ["+refs/heads/%s:refs/base/default" % default]
        if remote_exists:
            specs.append("+refs/heads/%s:refs/base/remote" % branch)
        rc, _, err = sb.git(["fetch", "--no-tags", "--no-recurse-submodules", url] + specs,
                            token, timeout=90)
        if rc != 0:
            raise Refusal("error: fetch from remote failed: " + clip(err, 300))

    @staticmethod
    def _is_github_path(p):
        p = p.lower()
        return p == ".github" or p.startswith(".github/")

    def _diff_paths(self, sb, a, b):
        rc, out, _ = sb.git(["diff", "--name-only", "--no-renames", "--no-ext-diff",
                             "--ignore-submodules=none", "-z", a, b])
        if rc != 0:
            raise Refusal("error: cannot diff against remote default branch")
        return [p for p in out.split("\0") if p]

    def workflow_changes(self, sb, sha):
        """True if sha touches .github/** vs the remote default branch, in the
        overall diff or in any single commit, or if .github is a symlink/gitlink."""
        if any(self._is_github_path(p) for p in self._diff_paths(sb, "refs/base/default", sha)):
            return True
        rc, out, _ = sb.git(["ls-tree", "-z", sha])
        if rc != 0:
            raise Refusal("error: cannot list tree")
        for ent in out.split("\0"):
            meta, _, name = ent.partition("\t")
            if name.lower() == ".github" and meta.split(" ")[0] in ("120000", "160000"):
                return True
        rc, out, _ = sb.git(["rev-list", "--parents", "refs/base/default..%s" % sha])
        if rc != 0:
            raise Refusal("error: cannot list commits")
        lines = [l for l in out.splitlines() if l.strip()]
        if len(lines) > MAX_COMMITS:
            raise Refusal("refused: more than %d unpushed commits; cannot verify" % MAX_COMMITS)
        for line in lines:
            parts = line.split()
            commit, parents = parts[0], parts[1:] or [EMPTY_TREE]
            for par in parents:
                if any(self._is_github_path(p) for p in self._diff_paths(sb, par, commit)):
                    return True
        return False

    # ---- tools ----------------------------------------------------------
    def tool_push_branch(self, args):
        repo = self.check_origin()
        cur = self.current_branch()
        branch = args.get("branch", cur)
        valid_ref_name(branch, "branch")
        if branch != cur:
            raise Refusal("refused: branch '%s' is not the checked-out branch '%s'" % (branch, cur))
        if branch.lower() in [p.lower() for p in self.cfg.protected]:
            raise Refusal("refused: branch '%s' is protected; push a feature branch" % branch)
        rc, _, _ = self.wgit(["check-ref-format", "refs/heads/" + branch])
        if rc != 0:
            raise Refusal("refused: not a valid git branch name")
        objdir = self.workspace_objdir()
        sha = self.resolve_tip(branch)  # resolved ONCE; everything below uses the sha
        token = self.load_token()
        url = self.remote_url(repo)
        sb = Sandbox(self, objdir)
        try:
            default, exists = self.discover_remote(sb, url, token, branch)
            if branch == default:
                raise Refusal("refused: '%s' is the remote default branch" % branch)
            self.fetch_remote(sb, url, token, default, branch, exists)
            rc, _, _ = sb.git(["cat-file", "-e", sha + "^{commit}"])
            if rc != 0:
                raise Refusal("refused: branch tip commit is not available")
            remote_tip = ""
            if exists:
                rc, out, _ = sb.git(["rev-parse", "--verify", "--quiet",
                                     "refs/base/remote^{commit}"])
                remote_tip = out.strip()
                if rc != 0 or not SHA_RE.fullmatch(remote_tip):
                    raise Refusal("error: cannot resolve the fetched remote branch")
                rc, _, _ = sb.git(["merge-base", "--is-ancestor", remote_tip, sha])
                if rc == 1:
                    raise Refusal("error: push rejected (non-fast-forward); fetch and rebase "
                                  "onto the remote branch, then push again (force is not allowed)")
                if rc != 0:
                    raise Refusal("error: cannot verify fast-forward")
            if not self.cfg.allow_workflow_changes and self.workflow_changes(sb, sha):
                raise Refusal("refused: .github/workflows changed; ask the user")
            dest = "refs/heads/%s" % branch
            rc, out, err = sb.git(
                ["push", "--porcelain", "--no-verify", "--no-follow-tags",
                 "--no-recurse-submodules",
                 "--force-with-lease=%s:%s" % (dest, remote_tip), url,
                 "%s:%s" % (sha, dest)], token, timeout=90)
        finally:
            sb.close()
        lines = [l for l in out.splitlines()
                 if l.strip() and not l.startswith(("To ", "Done"))]
        summary = clip("\n".join(lines) or "(no porcelain output)")
        low = (out + "\n" + err).lower()
        if rc != 0 or any(l.startswith("!") for l in lines):
            if "non-fast-forward" in low or "fetch first" in low or "[rejected]" in low:
                raise Refusal("error: push rejected (non-fast-forward or remote changed); "
                              "fetch and rebase onto the remote branch, then push again "
                              "(force is not allowed)")
            raise Refusal("error: push failed: " + clip(err or out, 600).replace("\n", " | "))
        return "pushed %s:%s\n%s" % (sha, dest, summary)

    def _api_json(self, path, token, timeout):
        data = self.gh(["api", "-H", "Accept: application/vnd.github+json", path],
                       token, timeout)
        try:
            return json.loads(data)
        except ValueError:
            raise Refusal("error: unexpected response from gh")

    def _poll_checks(self, repo, ref, token, timeout):
        """Check runs for ref, as dicts with name/status/conclusion.

        Fine-grained personal access tokens cannot be granted the Checks
        permission, so the check-runs endpoint answers 403 for them. In that
        case the same per-job detail is read from the Actions API (Actions:
        read) and merged with commit statuses (Commit statuses: read).
        """
        q = urllib.parse.quote(ref, safe="")
        if getattr(self, "_checks_mode", "check-runs") == "check-runs":
            try:
                data = self._api_json("repos/%s/commits/%s/check-runs?per_page=100"
                                      % (repo, q), token, timeout)
                runs = data.get("check_runs", []) if isinstance(data, dict) else None
                if not isinstance(runs, list):
                    raise Refusal("error: unexpected response from gh")
                return runs
            except Refusal as e:
                if "403" not in str(e) and "not accessible" not in str(e).lower():
                    raise
                self._checks_mode = "actions"
        commit = self._api_json("repos/%s/commits/%s" % (repo, q), token, timeout)
        sha = commit.get("sha") if isinstance(commit, dict) else None
        if not isinstance(sha, str) or not SHA_RE.fullmatch(sha.lower()):
            raise Refusal("error: cannot resolve ref to a commit")
        runs = []
        wf = self._api_json("repos/%s/actions/runs?head_sha=%s&per_page=100"
                            % (repo, sha), token, timeout)
        for run in (wf.get("workflow_runs") or [])[:20] if isinstance(wf, dict) else []:
            rid = run.get("id")
            if not isinstance(rid, int):
                continue
            jobs = self._api_json("repos/%s/actions/runs/%d/jobs?per_page=100"
                                  % (repo, rid), token, timeout)
            for job in (jobs.get("jobs") or []) if isinstance(jobs, dict) else []:
                runs.append({"name": job.get("name"), "status": job.get("status"),
                             "conclusion": job.get("conclusion")})
        st = self._api_json("repos/%s/commits/%s/status" % (repo, sha), token, timeout)
        for s in (st.get("statuses") or []) if isinstance(st, dict) else []:
            state = s.get("state")
            runs.append({"name": s.get("context"),
                         "status": "in_progress" if state == "pending" else "completed",
                         "conclusion": {"success": "success", "pending": None}.get(state, "failure")})
        return runs

    def gh(self, args, token, timeout, extra_env=None):
        tmp = tempfile.mkdtemp(prefix="ghagent-gh-")
        try:
            os.chmod(tmp, 0o700)
            env = self._base_env()
            env.update({"GH_TOKEN": token, "GH_CONFIG_DIR": tmp, "HOME": tmp,
                        "GH_PROMPT_DISABLED": "1", "NO_COLOR": "1",
                        "GH_NO_UPDATE_NOTIFIER": "1", "GH_HOST": "github.com"})
            if extra_env:
                env.update(extra_env)
            if not self.gh_path:
                raise Refusal("error: gh not found on PATH")
            rc, out, err = self._run([self.gh_path] + list(args), env, max(1, int(timeout)),
                                     token, cwd=tmp)
        finally:
            shutil.rmtree(tmp, ignore_errors=True)
        if rc != 0:
            raise Refusal("error: gh failed: " + clip(err or out, 400).replace("\n", " | "))
        return out

    @staticmethod
    def summarize(runs):
        counts, failed, pending = {}, [], []
        for r in runs:
            if r.get("status") != "completed":
                pending.append(str(r.get("name")))
                key = str(r.get("status"))
            else:
                key = str(r.get("conclusion"))
                if key in FAIL_CONCLUSIONS:
                    failed.append(str(r.get("name")))
            counts[key] = counts.get(key, 0) + 1
        return counts, failed, pending

    def tool_wait_for_checks(self, args):
        repo = self.check_origin()
        timeout = args.get("timeout_s", 90)
        if isinstance(timeout, bool) or not isinstance(timeout, int):
            raise Refusal("refused: timeout_s must be an integer")
        timeout = max(1, min(100, timeout))
        token = self.load_token()
        pr = args.get("pr")
        if pr is not None:
            if isinstance(pr, bool) or not isinstance(pr, int) or pr < 1:
                raise Refusal("refused: pr must be a positive integer")
            start = time.monotonic()
            raw = self.gh(["api", "repos/%s/pulls/%d" % (repo, pr)], token, 20)
            try:
                ref = json.loads(raw)["head"]["sha"]
            except (ValueError, KeyError, TypeError):
                raise Refusal("error: cannot determine PR head sha")
            valid_ref_name(ref, "sha")
        else:
            start = time.monotonic()
            ref = args.get("ref") or self.current_branch()
            valid_ref_name(ref, "ref")
        deadline = min(start + timeout, (self._deadline or (start + 100)) - 3)
        runs, complete = [], False
        while True:
            now = time.monotonic()
            runs = self._poll_checks(repo, ref, token, 15)
            complete = bool(runs) and all(r.get("status") == "completed" for r in runs)
            now = time.monotonic()
            if complete or now >= deadline:
                break
            time.sleep(max(0.0, min(self.cfg.poll_interval, deadline - now)))
        counts, failed, pending = self.summarize(runs)
        res = {"ref": ref, "complete": complete, "total": len(runs),
               "counts": counts, "failed": failed[:30], "pending": pending[:30],
               "waited_s": round(time.monotonic() - start, 1)}
        if not runs:
            res["note"] = "no check runs reported yet"
        return json.dumps(res, separators=(",", ":"))

    def tool_create_release(self, args):
        if not self.cfg.allow_releases:
            raise Refusal("refused: releases are disabled")
        repo = self.check_origin()
        tag = args.get("tag")
        sha = args.get("target_sha")
        notes = args.get("notes", "")
        draft = args.get("draft", True)
        valid_ref_name(tag, "tag")
        if not self.cfg.tag_re.fullmatch(tag):
            raise Refusal("refused: tag does not match the allowed tag pattern")
        if not isinstance(sha, str) or not SHA_RE.fullmatch(sha.lower()):
            raise Refusal("refused: target_sha must be a full 40-hex commit sha")
        sha = sha.lower()
        if not isinstance(notes, str) or len(notes) > 20000:
            raise Refusal("refused: notes must be a string of at most 20000 chars")
        if not isinstance(draft, bool):
            raise Refusal("refused: draft must be a boolean")
        token = self.load_token()
        url = self.remote_url(repo)
        sb = Sandbox(self)
        try:
            rc, out, err = sb.git(["ls-remote", "--tags", url, "refs/tags/" + tag],
                                  token, timeout=40)
            if rc != 0:
                raise Refusal("error: cannot reach remote: " + clip(err, 300))
            if out.strip():
                raise Refusal("refused: tag already exists on the remote")
            default = self.remote_default_branch(sb, url, token)
        finally:
            sb.close()
        # Ancestry is decided by GitHub, not by local (model-writable) git state.
        raw = self.gh(["api", "-H", "Accept: application/vnd.github+json",
                       "repos/%s/compare/%s...%s" % (repo, default, sha)], token, 30)
        try:
            status = json.loads(raw).get("status")
        except (ValueError, AttributeError):
            raise Refusal("error: unexpected response from gh")
        if status not in ("behind", "identical"):
            raise Refusal("refused: target_sha is not reachable from the remote default branch")
        runs = self._poll_checks(repo, sha, token, 20)
        counts, failed, pending = self.summarize(runs)
        if not runs:
            raise Refusal("refused: no check runs found for target_sha")
        # Skipped and neutral jobs are normal (path filters, docs-only rows);
        # anything failing or still running blocks, and something must pass.
        if failed or pending or not set(counts) <= {"success", "skipped", "neutral"} \
                or not counts.get("success"):
            raise Refusal("refused: checks are not all successful: " +
                          json.dumps(counts, separators=(",", ":")))
        fd, notes_path = tempfile.mkstemp(prefix="ghagent-notes-")
        try:
            with os.fdopen(fd, "w") as f:
                f.write(notes)
            cmd = ["release", "create", tag, "--repo", repo, "--target", sha,
                   "--title", tag, "--notes-file", notes_path]
            if draft:
                cmd.append("--draft")
            out = self.gh(cmd, token, 60)
        finally:
            try:
                os.unlink(notes_path)
            except OSError:
                pass
        return "created %srelease %s: %s" % ("draft " if draft else "", tag, clip(out, 300))

    # ---- tool registry --------------------------------------------------
    SCHEMAS = {
        "push_branch": {
            "description": "Push the checked-out branch to the pinned GitHub repo "
                           "(no force, never a protected branch).",
            "properties": {"branch": {"type": "string"}}, "required": []},
        "wait_for_checks": {
            "description": "Wait (max ~100s) for CI check runs of a ref or PR and "
                           "return a compact summary.",
            "properties": {"ref": {"type": "string"}, "pr": {"type": "integer"},
                           "timeout_s": {"type": "integer", "minimum": 1, "maximum": 100}},
            "required": []},
        "create_release": {
            "description": "Create a GitHub release (draft by default) for a "
                           "commit on the default branch whose checks all passed.",
            "properties": {"tag": {"type": "string"}, "target_sha": {"type": "string"},
                           "notes": {"type": "string", "maxLength": 20000},
                           "draft": {"type": "boolean"}},
            "required": ["tag", "target_sha"]},
    }

    def tools_list(self):
        out = []
        for name, s in self.SCHEMAS.items():
            if name == "create_release" and not self.cfg.allow_releases:
                continue
            out.append({"name": name, "description": s["description"],
                        "inputSchema": {"type": "object",
                                        "properties": s["properties"],
                                        "required": s["required"],
                                        "additionalProperties": False}})
        return out

    def call_tool(self, name, args):
        """Return (text, is_error). Raises KeyError for unknown tools."""
        if name not in self.SCHEMAS or (name == "create_release" and not self.cfg.allow_releases):
            raise KeyError(name)
        self.begin_call()
        try:
            if not isinstance(args, dict):
                raise Refusal("refused: arguments must be an object")
            extra = set(args) - set(self.SCHEMAS[name]["properties"])
            if extra:
                raise Refusal("refused: unknown argument(s): " + ", ".join(sorted(map(str, extra)))[:100])
            for k in self.SCHEMAS[name]["required"]:
                if k not in args:
                    raise Refusal("refused: missing argument '%s'" % k)
            for k, v in args.items():
                t = self.SCHEMAS[name]["properties"][k]["type"]
                ok = (isinstance(v, str) if t == "string" else
                      isinstance(v, bool) if t == "boolean" else
                      isinstance(v, int) and not isinstance(v, bool))
                if not ok:
                    raise Refusal("refused: argument '%s' must be of type %s" % (k, t))
            return getattr(self, "tool_" + name)(args), False
        except Refusal as e:
            return str(e), True
        except Exception as e:  # never leak details beyond the type
            log("internal error in %s: %s" % (name, type(e).__name__))
            return "error: internal error (%s)" % type(e).__name__, True

    # ---- JSON-RPC -------------------------------------------------------
    def handle(self, msg):
        """Return a response dict or None."""
        if not isinstance(msg, dict) or msg.get("jsonrpc") != "2.0" \
                or not isinstance(msg.get("method"), str):
            if isinstance(msg, dict) and "id" in msg and "method" not in msg:
                return None  # a response to us; ignore
            return {"jsonrpc": "2.0", "id": None,
                    "error": {"code": -32600, "message": "Invalid Request"}}
        method, mid = msg["method"], msg.get("id")
        is_note = "id" not in msg
        params = msg.get("params") or {}
        if not isinstance(params, dict):
            params = {}

        def ok(result):
            return None if is_note else {"jsonrpc": "2.0", "id": mid, "result": result}

        def err(code, text):
            return None if is_note else {"jsonrpc": "2.0", "id": mid,
                                         "error": {"code": code, "message": text}}
        if method == "initialize":
            pv = params.get("protocolVersion")
            return ok({"protocolVersion": pv if isinstance(pv, str) else DEFAULT_PROTOCOL,
                       "serverInfo": {"name": SERVER_NAME, "version": SERVER_VERSION},
                       "capabilities": {"tools": {}}})
        if method.startswith("notifications/"):
            return None
        if method == "ping":
            return ok({})
        if method == "tools/list":
            return ok({"tools": self.tools_list()})
        if method == "tools/call":
            name = params.get("name")
            try:
                text, is_err = self.call_tool(name, params.get("arguments") or {})
            except (KeyError, TypeError):
                return err(-32602, "Unknown tool")
            return ok({"content": [{"type": "text", "text": text}], "isError": is_err})
        return err(-32601, "Method not found")

    def serve(self, stdin=None, stdout=None):
        stdin = stdin or sys.stdin
        stdout = stdout or sys.stdout
        for line in stdin:
            line = line.strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
            except ValueError:
                resp = {"jsonrpc": "2.0", "id": None,
                        "error": {"code": -32700, "message": "Parse error"}}
            else:
                try:
                    resp = self.handle(msg)
                except Exception as e:
                    log("handler error: %s" % type(e).__name__)
                    resp = {"jsonrpc": "2.0", "id": msg.get("id") if isinstance(msg, dict) else None,
                            "error": {"code": -32603, "message": "Internal error"}}
            if resp is not None:
                stdout.write(json.dumps(resp, separators=(",", ":")) + "\n")
                stdout.flush()


def parse_args(argv):
    ap = argparse.ArgumentParser(prog=SERVER_NAME, description="Security-focused GitHub MCP server")
    ap.add_argument("--allow-repo", action="append", required=True, metavar="OWNER/REPO")
    ap.add_argument("--protected", action="append", metavar="BRANCH",
                    help="additional protected branch (main and master are always protected)")
    ap.add_argument("--allow-workflow-changes", action="store_true")
    ap.add_argument("--allow-releases", action="store_true")
    ap.add_argument("--tag-pattern", default=r"v[0-9]+\.[0-9]+\.[0-9]+")
    g = ap.add_mutually_exclusive_group(required=True)
    g.add_argument("--token-file", metavar="PATH")
    g.add_argument("--token-env", metavar="NAME")
    # TEST-ONLY options (documented in README); not for production use.
    ap.add_argument("--remote-url-override", help=argparse.SUPPRESS)
    ap.add_argument("--poll-interval", type=float, default=10.0, help=argparse.SUPPRESS)
    a = ap.parse_args(argv)
    for r in a.allow_repo:
        if not OWNER_REPO_RE.fullmatch(r):
            ap.error("--allow-repo must look like OWNER/REPO")
    try:
        re.compile(a.tag_pattern)
    except re.error:
        ap.error("invalid --tag-pattern")
    return Config(a.allow_repo, a.protected or [],
                  a.allow_workflow_changes, a.allow_releases, a.tag_pattern,
                  a.token_file, a.token_env, a.remote_url_override, a.poll_interval)


def main(argv=None):
    cfg = parse_args(sys.argv[1:] if argv is None else argv)
    try:
        srv = Server(cfg)
    except StartupError as e:
        log("cannot start: %s" % e)
        return 2
    srv.serve()
    return 0


if __name__ == "__main__":
    sys.exit(main())
