# Remote view of a running session (`remote` feature)

`src/extras/remote/` is a small HTTP/SSE server: `GET /` serves one static page, `GET /events`
streams session events, `POST /steer`, `/permission` and `/cancel` hand browser commands to the session
through `RemoteServer::commands`. It binds loopback unless `allow_non_loopback` is set (there is no TLS, so
anything wider belongs behind an SSH tunnel or TLS proxy), needs the random per-run token on every request
(`Authorization: Bearer` or `?token=`), checks the `Host` header, bounds headers/bodies/connections, and
accepts each permission answer once for a still-pending ask.

Status: the server and its tests exist; the TUI is not yet wired to it (no `serve` command or
`--remote-control` flag), so nothing starts it today.
