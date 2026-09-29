# Security Policy

mini-agent runs model-directed tools on your machine, so its sandbox, permission and JavaScript
containment boundaries are security boundaries. Please report weaknesses in them privately.

## Supported versions

Security fixes are made on `main` and shipped in the next release of the **latest minor version**.
Older minor versions do not receive backported fixes; upgrade to the newest release listed on the
[releases page](https://github.com/sebahrens/mini-agent/releases).

| Version                   | Supported |
| ------------------------- | --------- |
| Latest minor (e.g. 1.9.x) | Yes       |
| Any earlier minor         | No        |

## Reporting a vulnerability

**Do not open a public issue, discussion or pull request for a security problem.**

Report it through GitHub private vulnerability reporting:

<https://github.com/sebahrens/mini-agent/security/advisories/new>

(On the repository page: **Security** tab, then **Report a vulnerability**.) This creates a private
advisory visible only to you and the maintainers. There is no separate security email address.

Please include:

- the mini-agent version (`mini-agent --version`), operating system and architecture;
- the sandbox backend and permission mode in use, and any relevant configuration;
- the build features if you built from source (for example `--no-default-features`);
- a minimal reproduction and what boundary it crosses (what you could read, write, execute or reach
  that the documented policy says you should not).

## Response targets

These are goals for a volunteer-maintained project, not contractual guarantees:

| Stage                                      | Target                         |
| ------------------------------------------ | ------------------------------ |
| Acknowledge the report                     | within 3 business days         |
| Initial assessment (validity, severity)    | within 10 business days        |
| Fix or mitigation for high/critical issues | within 30 days of confirmation |
| Fix for lower-severity issues              | in a following regular release |

We will keep you updated in the advisory, credit you in the advisory and `CHANGELOG.md` unless you
prefer otherwise, and publish the advisory once a fixed release is available. Please give us a
reasonable chance to ship a fix before disclosing publicly.

## Scope

In scope:

- **Sandbox escapes** of the general subprocess sandbox (`bwrap`, `seatbelt`, `appcontainer`) when
  it is enabled: reading, writing or executing outside the documented policy.
- **Permission bypasses**: an operation that runs without the approval its permission mode and
  configured `allow`/`ask`/`deny` rules require, including hook approval and content binding.
- **JavaScript containment**: escapes from the contained JS worker or its parent broker, or any
  violation of the
  [Phase 6 security invariants](docs/specs/phase-6-brokered-js-runtime.md).
- **Credential handling**: exposure of stored API keys or OAuth tokens to sandboxed commands, model
  output, logs, session files or other processes, beyond what the documentation describes.
- Terminal escape injection, unsafe handling of untrusted workspace files (`AGENTS.md`, hooks,
  skills, MCP/ACP input), and release or supply-chain integrity issues (checksums, provenance,
  Corresponding Source).

Out of scope (documented behaviour, not vulnerabilities):

- Effects of **`--no-sandbox`** (or `sandbox = false`): subprocesses intentionally run unsandboxed.
- Effects of **`--yolo`** and **`--dangerously-skip-permissions`**: operations are intentionally
  allowed without asking, as described in
  [Get started](docs/agent/GET_STARTED.md) and [Configuration](docs/agent/CONFIG.md). A bypass of
  what `--yolo` still enforces (configured `deny` rules and its destructive-command prompt) is in
  scope.
- Limits the documentation already states, such as the AppContainer caveats in `CONFIG.md` or a
  backend that is unavailable on the host and was not explicitly required.
- Actions you explicitly approved, a model following instructions you gave it, and vulnerabilities
  in third-party model providers, MCP servers or other tools you choose to run.
- The research-only `spike/` crate, which is never shipped.

## Maintainer note

Private vulnerability reporting must stay enabled for the reporting link above to work. It was
enabled on 2026-09-29 with:

```bash
gh api -X PUT repos/sebahrens/mini-agent/private-vulnerability-reporting
gh api repos/sebahrens/mini-agent/private-vulnerability-reporting   # {"enabled":true}
```

If that call is refused (it needs repository admin rights), enable it manually under **Settings**,
**Code security**, **Private vulnerability reporting**, **Enable**. For a fork or a transferred
repository, repeat this step and update the links in this file.
