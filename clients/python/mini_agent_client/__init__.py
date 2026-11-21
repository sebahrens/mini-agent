"""Client for `mini-agent -p --input-format stream-json --output stream-json --stream-json-control`.

Wire contract: docs/specs/stream-json-control.md. Unknown record fields are ignored.
"""
from __future__ import annotations

import json
import os
import subprocess
import threading
import queue
from typing import Any, Callable, Dict, Iterator, List, Optional

PROTOCOL = "mini-agent-control/1"

PermissionCallback = Callable[[str, Any, Dict[str, Any]], Dict[str, Any]]

_END = object()


def build_args(
    prompt: Optional[str] = None,
    schema: str = "claude",
    include_partial_messages: bool = False,
    ask_timeout_secs: Optional[int] = None,
    preview_bytes: Optional[int] = None,
    model: Optional[str] = None,
    permission_mode: Optional[str] = None,
    continue_session: bool = False,
    extra_args: Optional[List[str]] = None,
) -> List[str]:
    args = ["-p"]
    if prompt:
        args.append(prompt)
    args += ["--input-format", "stream-json", "--output", "stream-json", "--stream-json-control"]
    args += ["--stream-json-schema", schema]
    if include_partial_messages:
        args.append("--include-partial-messages")
    if ask_timeout_secs is not None:
        args += ["--stream-json-ask-timeout", str(ask_timeout_secs)]
    if preview_bytes is not None:
        args += ["--stream-json-preview-bytes", str(preview_bytes)]
    if model:
        args += ["--model", model]
    if permission_mode:
        args += ["--permission-mode", permission_mode]
    if continue_session:
        args.append("--continue")
    return args + list(extra_args or [])


class ControlError(RuntimeError):
    """A control request was acknowledged with an `error` response."""


class Session:
    """One headless session. `messages()` yields every record, `can_use_tool` asks go to the callback."""

    def __init__(
        self,
        binary: str = "mini-agent",
        *,
        prompt: Optional[str] = None,
        can_use_tool: Optional[PermissionCallback] = None,
        cwd: Optional[str] = None,
        env: Optional[Dict[str, str]] = None,
        argv: Optional[List[str]] = None,
        **options: Any,
    ) -> None:
        self._can_use_tool = can_use_tool
        self._next_id = 1
        self._pending: Dict[str, "queue.Queue[Any]"] = {}
        self._lock = threading.Lock()
        self._records: "queue.Queue[Any]" = queue.Queue()
        full_env = dict(os.environ)
        full_env.update(env or {})
        command = [binary] + (argv if argv is not None else build_args(prompt=prompt, **options))
        self._proc = subprocess.Popen(
            command,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            cwd=cwd,
            env=full_env,
            text=True,
            encoding="utf-8",
            bufsize=1,
        )
        self._reader = threading.Thread(target=self._read, daemon=True)
        self._reader.start()

    def _read(self) -> None:
        assert self._proc.stdout is not None
        for line in self._proc.stdout:
            try:
                record = json.loads(line)
            except ValueError:
                continue
            kind = record.get("type")
            if kind == "control_response":
                body = record.get("response") or {}
                with self._lock:
                    waiter = self._pending.pop(body.get("request_id"), None)
                if waiter is not None:
                    waiter.put(body)
            elif kind == "control_request" and (record.get("request") or {}).get("subtype") == "can_use_tool":
                threading.Thread(target=self._answer_permission, args=(record,), daemon=True).start()
            else:
                self._records.put(record)
        with self._lock:
            waiters = list(self._pending.values())
            self._pending.clear()
        for waiter in waiters:
            waiter.put(None)
        self._records.put(_END)

    def _answer_permission(self, record: Dict[str, Any]) -> None:
        reply: Dict[str, Any] = {"behavior": "deny", "message": "no can_use_tool callback"}
        if self._can_use_tool is not None:
            ask = record["request"]
            try:
                reply = self._can_use_tool(ask.get("tool_name", ""), ask.get("tool_input", ask.get("input")), ask)
            except Exception as error:  # a failing callback must never allow
                reply = {"behavior": "deny", "message": str(error)}
        self._write({
            "type": "control_response",
            "response": {"subtype": "success", "request_id": record["request_id"], "response": reply},
        })

    def _write(self, value: Dict[str, Any]) -> bool:
        stdin = self._proc.stdin
        if stdin is None or stdin.closed or self._proc.poll() is not None:
            return False
        try:
            with self._lock:
                stdin.write(json.dumps(value) + "\n")
                stdin.flush()
        except (BrokenPipeError, ValueError, OSError):
            return False
        return True

    def send(self, text: str) -> bool:
        """Sends one user message; its turn ends with a `result` record."""
        return self._write({"type": "user", "message": {"role": "user", "content": text}})

    def control(self, subtype: str, **fields: Any) -> Dict[str, Any]:
        with self._lock:
            request_id = f"c{self._next_id}"
            self._next_id += 1
            waiter: "queue.Queue[Any]" = queue.Queue(maxsize=1)
            self._pending[request_id] = waiter
        sent = self._write({
            "type": "control_request",
            "request_id": request_id,
            "request": {"subtype": subtype, **fields},
        })
        if not sent:
            with self._lock:
                self._pending.pop(request_id, None)
            raise ControlError("mini-agent is not running")
        body = waiter.get(timeout=30)
        if body is None:
            raise ControlError("mini-agent exited")
        if body.get("subtype") != "success":
            raise ControlError(body.get("error", "control request failed"))
        return body.get("response") or {}

    def initialize(self) -> Dict[str, Any]:
        return self.control("initialize")

    def interrupt(self) -> Dict[str, Any]:
        return self.control("interrupt")

    def set_model(self, model: str) -> Dict[str, Any]:
        return self.control("set_model", model=model)

    def set_permission_mode(self, mode: str) -> Dict[str, Any]:
        return self.control("set_permission_mode", mode=mode)

    def messages(self) -> Iterator[Dict[str, Any]]:
        while True:
            record = self._records.get()
            if record is _END:
                self._records.put(_END)
                return
            yield record

    def turn(self) -> Iterator[Dict[str, Any]]:
        """Yields records up to and including the next `result`."""
        for record in self.messages():
            yield record
            if record.get("type") == "result":
                return

    def close(self) -> int:
        if self._proc.stdin is not None and not self._proc.stdin.closed:
            try:
                self._proc.stdin.close()
            except OSError:
                pass
        code = self._proc.wait()
        self._reader.join(timeout=5)
        if self._proc.stdout is not None:
            self._proc.stdout.close()
        return code

    def __enter__(self) -> "Session":
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()


def query(prompt: str, binary: str = "mini-agent", **options: Any) -> Iterator[Dict[str, Any]]:
    """Runs one prompt to completion, yielding its records and ending the session."""
    with Session(binary, prompt=prompt, **options) as session:
        yield from session.turn()
