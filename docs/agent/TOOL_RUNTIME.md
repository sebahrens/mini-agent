# Tool Runtime

Built-in, optional, and MCP tools are collected as `ToolDyn` instances before
the main agent is built. After command-line filtering, each registered tool is
wrapped by `src/agent/tools/memoize.rs`. The wrapper snapshots the tool name,
description, and JSON parameter schema once, then returns owned clones when Rig
requests provider definitions on later completion turns. Calls and structured
results are delegated unchanged to the original tool.

The same wrapper is used for read-only `/btw` and exploration-subagent tool
sets. Definition metadata that genuinely needs to vary while an agent is live
must not be placed behind this wrapper; current tool metadata is fixed when its
agent instance is constructed.
