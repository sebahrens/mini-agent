# ghagent-mcp

Small stdio MCP server (Python 3 stdlib only) giving a sandboxed coding agent the few
GitHub operations it cannot do itself: `push_branch`, `wait_for_checks`, and optionally
`create_release`. It runs OUTSIDE the sandbox as trusted code; its own input validation
is the safety boundary. All configuration comes from argv, never from the agent.

## Install and wire up
`make install` (from the repo root) runs `make install-ghagent` first: it copies `ghagent_mcp.py` to
`$PREFIX/share/ghagent-mcp/` and the `ghagent-mcp` launcher plus `ghagent-save-token` to `$PREFIX/bin/`
(`PREFIX` defaults to `~/.local`). It does not touch `~/.config/ghagent` or your mini-agent config.

1. `ghagent-save-token` stores a fine-grained PAT in `~/.config/ghagent/token` (mode 600). Scope the
   token to exactly the repos you allow below (Contents and Checks read/write as needed).
2. Register the server in `~/.config/mini-agent/config.toml`; the allowlist lives here, not on GitHub:
   ```toml
   [mcp_servers.ghagent]
   command = "/home/you/.local/bin/ghagent-mcp"
   args = ["--allow-repo", "OWNER/REPO", "--token-file", "/home/you/.config/ghagent/token"]
   inherit_env = ["HOME", "PATH"]
   ```
   Adding a repo means adding another `--allow-repo OWNER/REPO` pair and restarting mini-agent. A push
   from a checkout whose `origin` is not listed is refused with `origin is not an allowed repository`.

## Options
- `--allow-repo OWNER/REPO` (repeatable, required): `origin` of the cwd must resolve to one.
- `--protected BRANCH` (repeatable): ADDS to the built-in protected set; `main` and `master`
  are always protected and cannot be removed.
- `--allow-workflow-changes`: off by default; otherwise push is refused if the overall diff
  or any single commit in `<remote default>..<tip>` touches `.github` or anything under
  `.github/` (case-insensitive: workflows, actions, CODEOWNERS, symlink replacements), or if
  the tip tree has `.github` as a symlink or gitlink.
- `--allow-releases`, `--tag-pattern REGEX` (default `v[0-9]+\.[0-9]+\.[0-9]+`, matched with
  fullmatch).
- `--token-file PATH` (preferred; regular file, no group/world bits) or `--token-env NAME`.
- Test-only (hidden): `--remote-url-override URL` replaces the pinned push URL so tests can
  use a local bare repo; `--poll-interval SECONDS`. Never use in production.

The token reaches git only via `GIT_CONFIG_*` env (extraheader) and gh only via `GH_TOKEN`
with a fresh private `GH_CONFIG_DIR`; it is never in argv, and is redacted from captured output.

## Threat model and design
The workspace (working tree and most of `.git`) is model-controlled, so no decision is made
from workspace repo state other than the object store itself (content-addressed).
- At startup `git` and `gh` are resolved to absolute paths over a PATH stripped of empty and
  relative entries (a workspace `./git` can never run); startup fails if git is missing.
- `push_branch` reads from the workspace only: checked-out branch name, origin URL, the object
  directory, and the branch tip, resolved once to a sha with replace objects disabled. A
  workspace with `.git/info/grafts` or `.git/shallow` is refused.
- Every call then works in a private 0700 temp bare repo (`GIT_DIR`), which has no grafts,
  shallow file, replace refs, config or hooks, with the workspace object dir attached via
  `GIT_ALTERNATE_OBJECT_DIRECTORIES` and `GIT_NO_REPLACE_OBJECTS=1`. The default branch
  (`refs/base/default`) and the remote branch (`refs/base/remote`) are fetched from the
  pinned URL into it; the fast-forward check (`merge-base --is-ancestor`) and the
  `.github` check run there; the temp repo is deleted afterwards.
- Push is `<sha>:refs/heads/<branch>` from the temp repo with
  `--force-with-lease=refs/heads/<branch>:<observed remote tip or empty>`: the server-side
  update is atomic against the tip that was verified, and, given the fast-forward check, is
  never a rewrite. The pinned URL (never `origin`) is used, no tags, no submodule recursion.
- `create_release` asks GitHub (`gh api repos/O/R/compare/<default>...<sha>`, status must be
  `behind` or `identical`) instead of trusting local git; all check runs must be `success`;
  draft defaults to true.
- All subprocesses run in their own session; on timeout the whole process group is killed.
  Each tool call has a 100 s overall deadline; each step gets `min(step_timeout, remaining)`.
- All validation regexes use `fullmatch` (no trailing-newline bypass).
- `wait_for_checks` polls `gh api repos/O/R/commits/<ref>/check-runs` (PR numbers resolve to
  the head sha first) every 10 s, up to the call deadline; first 100 check runs only.

Known limits: the workspace `.git` must be (a realpath) inside the workspace, so linked
worktrees and submodule-style external gitdirs are refused. A workspace object store that is
missing objects for the tip makes the push fail rather than fetch them.

## Tests
`python3 -I -m unittest` from this directory.
