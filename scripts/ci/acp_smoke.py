#!/usr/bin/env python3
"""Drive `mini-agent --acp` the way each documented editor configuration launches it.

For every client in ARGV_BY_CLIENT the script starts the binary with exactly that argv,
sends initialize, session/new and one session/prompt over stdio (newline-delimited
JSON-RPC) against the offline stub provider, and checks the stub's answer arrives as
an agent message chunk. docs/agent/EDITORS.md must show the same argv; the unit test
scripts/tests/test_ci_docs_guides.py enforces that.
"""

from __future__ import annotations

import argparse
import json
import os
import queue
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

ARGV_BY_CLIENT = {
    "zed": ["--acp"],
    "jetbrains": ["--acp"],
    "neovim": ["--acp"],
    "vscode": ["--acp"],
}

ANSWER = "STUB-REVIEW-OK"
HERE = Path(__file__).resolve().parent


def start_stub(workdir: Path) -> tuple[subprocess.Popen, int]:
    port_file = workdir / "stub.port"
    process = subprocess.Popen(
        [sys.executable, str(HERE / "stub_provider.py"), "--port-file", str(port_file)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    for _ in range(100):
        if port_file.exists() and port_file.read_text().strip():
            return process, int(port_file.read_text())
        time.sleep(0.1)
    process.kill()
    raise SystemExit("stub provider did not start")


def write_config(config_dir: Path, port: int) -> None:
    config_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
    os.chmod(config_dir, 0o700)
    config = config_dir / "config.toml"
    config.write_text(
        "\n".join(
            [
                'provider = "stub"',
                'model = "stub"',
                "models_catalog_refresh_days = 0",
                "[custom_providers.stub]",
                'provider_type = "openai"',
                'api_style = "completions"',
                f'base_url = "http://127.0.0.1:{port}/v1"',
                'model = "stub"',
                "[custom_providers.stub.model_prices.stub]",
                "input_token_cost = 1.0",
                "output_token_cost = 2.0",
                "",
            ]
        ),
        encoding="utf-8",
    )
    os.chmod(config, 0o600)


class Peer:
    def __init__(self, binary: str, argv: list[str], env: dict[str, str], cwd: Path) -> None:
        self.child = subprocess.Popen(
            [binary, *argv],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
            cwd=cwd,
            text=True,
        )
        self.messages: queue.Queue = queue.Queue()
        self.notifications: list[dict] = []
        threading.Thread(target=self._read, daemon=True).start()
        threading.Thread(target=self.child.stderr.read, daemon=True).start()

    def _read(self) -> None:
        for line in self.child.stdout:
            line = line.strip()
            if line:
                try:
                    self.messages.put(json.loads(line))
                except json.JSONDecodeError:
                    pass

    def request(self, ident: int, method: str, params: dict, timeout: float = 60.0) -> dict:
        self.child.stdin.write(json.dumps({"jsonrpc": "2.0", "id": ident, "method": method, "params": params}) + "\n")
        self.child.stdin.flush()
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise SystemExit(f"timed out waiting for {method}")
            try:
                message = self.messages.get(timeout=remaining)
            except queue.Empty:
                raise SystemExit(f"timed out waiting for {method}") from None
            if message.get("id") == ident and "method" not in message:
                if "error" in message:
                    raise SystemExit(f"{method} failed: {message['error']}")
                return message["result"]
            self.notifications.append(message)

    def close(self) -> None:
        try:
            self.child.stdin.close()
            self.child.wait(timeout=10)
        except Exception:  # noqa: BLE001
            self.child.kill()


def smoke(client: str, binary: str, workdir: Path, port: int) -> None:
    config_dir = workdir / f"config-{client}"
    data_dir = workdir / f"data-{client}"
    data_dir.mkdir(mode=0o700, exist_ok=True)
    os.chmod(data_dir, 0o700)
    write_config(config_dir, port)
    workspace = workdir / f"ws-{client}"
    workspace.mkdir(exist_ok=True)
    env = {
        **os.environ,
        "MINI_AGENT_CONFIG_DIR": str(config_dir),
        "MINI_AGENT_DATA_DIR": str(data_dir),
    }
    peer = Peer(binary, ARGV_BY_CLIENT[client], env, workspace)
    try:
        initialized = peer.request(1, "initialize", {"protocolVersion": 1, "clientCapabilities": {}})
        assert initialized.get("agentCapabilities", {}).get("loadSession") is True, initialized
        session = peer.request(2, "session/new", {"cwd": str(workspace), "mcpServers": []})["sessionId"]
        result = peer.request(
            3,
            "session/prompt",
            {"sessionId": session, "prompt": [{"type": "text", "text": "Say hello."}]},
        )
        assert result.get("stopReason") == "end_turn", result
        text = "".join(
            note["params"]["update"].get("content", {}).get("text", "")
            for note in peer.notifications
            if note.get("method") == "session/update"
            and note["params"]["update"].get("sessionUpdate") == "agent_message_chunk"
        )
        assert ANSWER in text, f"no agent message with {ANSWER!r}: {peer.notifications}"
    finally:
        peer.close()
    print(f"{client}: ok (argv {ARGV_BY_CLIENT[client]})")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, help="path to the mini-agent executable")
    parser.add_argument("--client", action="append", choices=sorted(ARGV_BY_CLIENT), help="default: all")
    args = parser.parse_args()
    args.binary = str(Path(args.binary).resolve())
    with tempfile.TemporaryDirectory() as raw:
        workdir = Path(raw).resolve()
        stub, port = start_stub(workdir)
        try:
            for client in args.client or sorted(ARGV_BY_CLIENT):
                smoke(client, args.binary, workdir, port)
        finally:
            stub.kill()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
