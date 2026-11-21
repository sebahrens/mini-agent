#!/usr/bin/env node
// Thin launcher: finds the native binary in the per-platform optional dependency
// and runs it with the caller's argv, stdio and exit status. Nothing is downloaded
// at install or run time, so the release's SHA256SUMS and attestation still cover
// exactly the bytes that run.
"use strict";

const { spawn } = require("node:child_process");
const path = require("node:path");

const SCOPE = "@sebahrens";
const platform = `${process.platform}-${process.arch}`;
const supported = new Set([
  "darwin-arm64",
  "darwin-x64",
  "linux-arm64",
  "linux-x64",
  "win32-x64",
]);

function fail(message) {
  process.stderr.write(`mini-agent: ${message}\n`);
  process.exit(1);
}

if (!supported.has(platform)) {
  fail(
    `no prebuilt binary for ${platform}; install from https://github.com/sebahrens/mini-agent/releases or build with cargo`,
  );
}

const exe = process.platform === "win32" ? "mini-agent.exe" : "mini-agent";
let binary;
try {
  const manifest = require.resolve(`${SCOPE}/mini-agent-${platform}/package.json`);
  binary = path.join(path.dirname(manifest), exe);
} catch (error) {
  fail(
    `the ${SCOPE}/mini-agent-${platform} package is missing. Reinstall without --omit=optional ` +
      "(npm install -g @sebahrens/mini-agent).",
  );
}

const child = spawn(binary, process.argv.slice(2), { stdio: "inherit" });
child.on("error", (error) => fail(`could not start ${binary}: ${error.message}`));
for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"]) {
  process.on(signal, () => child.kill(signal));
}
child.on("exit", (code, signal) => {
  if (signal) {
    process.kill(process.pid, signal);
  } else {
    process.exit(code === null ? 1 : code);
  }
});
