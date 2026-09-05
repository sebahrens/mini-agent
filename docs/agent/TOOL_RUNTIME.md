# Tool Runtime

Built-in, optional, and MCP tools are collected as `ToolDyn` instances before
the main agent is built. After command-line filtering, each registered tool is
wrapped by `src/agent/tools/memoize.rs`. The wrapper snapshots the tool name,
description, and JSON parameter schema once, then returns owned clones when Rig
requests provider definitions on later completion turns. Calls and structured
results are delegated unchanged to the original tool.

Known limit: `/editsys` changes the global edit system without rebuilding the
agent, so the memoized `read`/`edit` definitions stay in the previous mode
until an unrelated rebuild (mini-agent-fcer).

Foreground shell commands run under a fixed 30 s deadline that a call can only
lower. A shell call with `background = true` instead returns a session-scoped
job id immediately and uses a 24-hour maximum (again lowerable with `timeout`).
The companion `job_status` tool polls bounded live output or stops and reaps
the job. A session permits at most eight concurrent jobs and retains the newest
32 job records; each stream keeps a 64 KiB rolling head/tail view. Background
commands use the same shell, workspace, sandbox, and permission decision as
foreground commands, and all are cancelled on turn cancellation or session
shutdown. Tool calls issued together in one assistant message execute
sequentially (mini-agent-zlva).

The same wrapper is used for read-only `/btw` and exploration-subagent tool
sets. Definition metadata that genuinely needs to vary while an agent is live
must not be placed behind this wrapper; current tool metadata is fixed when its
agent instance is constructed.
