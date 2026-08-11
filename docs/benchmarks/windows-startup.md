# Windows startup benchmark

Measure installed debug builds from a project directory, not the user-profile root. Use a prompt
that exits before a provider request so the result isolates local startup capability work. Run at
least five fresh processes for each mode and record median and maximum elapsed milliseconds.

```powershell
cargo install --path . --debug

1..5 | ForEach-Object {
  (Measure-Command { mini-agent --no-tools --print-config | Out-Null }).TotalMilliseconds
}

1..5 | ForEach-Object {
  (Measure-Command { mini-agent --tools read --print-config | Out-Null }).TotalMilliseconds
}

1..5 | ForEach-Object {
  (Measure-Command { mini-agent --print-config | Out-Null }).TotalMilliseconds
}
```

The `--no-tools` case must not run shell discovery, general AppContainer containment, JavaScript
worker containment, MCP connection, or learned-skill initialization. The `--tools read` case also
skips every process capability probe and MCP connection. The default case may run one
general and one JavaScript preflight; each result is process-local and fail-closed. The general
Windows preflight has a five-second run deadline, up to five seconds to reap the complete helper
tree, and a fresh five-second profile/ACL recovery ceiling. Capture the
closed `windows_general_appcontainer_preflight` timing event alongside total process time; do not
record paths, shell text, JavaScript diagnostics, or configuration contents.

Windows-native measurements are release evidence and must be appended here with the commit, host
class, five raw samples, median, and maximum. Measurements from macOS/Linux are not substitutes.
