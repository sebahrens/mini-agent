# Remote view of a running session (`remote` feature)

`src/extras/remote/` is a small HTTP/SSE server: `GET /` serves one static page, `GET /events`
streams session events, `POST /steer`, `/permission` and `/cancel` hand browser commands to the session
through `RemoteServer::commands`. It binds loopback unless `allow_non_loopback` is set (there is no TLS, so
anything wider belongs behind an SSH tunnel or TLS proxy), needs the random per-run token on every request
(`Authorization: Bearer` or `?token=`), checks the `Host` header, bounds headers/bodies/connections, and
accepts each permission answer once for a still-pending ask.

## Wiring into the TUI

`/remote on|off|status` (feature `remote`, see `docs/agent/COMMANDS.md`) is the only way to start
the server; nothing listens by default. `bridge.rs` holds the non-TUI logic and `src/ui/app.rs` the
thin glue:

- `bridge::parse_request` parses the command. A non-loopback `--host` is refused without
  `--allow-non-loopback`, and the flag alone never widens the default loopback bind;
  `non_loopback_warning` supplies the warning printed when the bind is wider.
- `bridge::EventMapper` turns `AgentEvent`s (token lines, tool calls and results, steering, errors,
  turn end) into the page's JSON events; text is sanitized and bounded.
- `bridge::RemoteLink::sync_ask` is called every event-loop iteration with the terminal's permission
  prompt (`PermissionState::remote_view`, keyed by when the prompt opened). It publishes the ask,
  replaces it when a new prompt opens and withdraws it when the terminal answered, so a late browser
  answer is refused by the server (404) before it reaches the session.
- Browser commands arrive through `RemoteLink::recv` and are applied by `App::apply_remote_command`:
  steer goes to the run's `Steering` handle (the `/steer` queue), a permission answer goes to
  `PermissionState::answer_remote` (allow once or deny, only if the prompt is still the one offered;
  the oneshot reply makes the first answer win), cancel runs `abort_main_run` like Ctrl-C.
  Prompts that offer a grant are not offered remotely.

The token is printed once, in the transcript of the terminal that started it, and is never shown by
`/remote status`.
