#!/usr/bin/env python3
"""Offline OpenAI-compatible chat-completions stub for the action self-test.

It answers every request with one fixed assistant message, streamed (SSE) when the
request asks for a stream and as one JSON body otherwise. It never calls out and
binds only to loopback.
"""

from __future__ import annotations

import argparse
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

ANSWER = "STUB-REVIEW-OK"
USAGE = {"prompt_tokens": 11, "completion_tokens": 3, "total_tokens": 14}


def completion_body(model: str) -> dict:
    return {
        "id": "stub-1",
        "object": "chat.completion",
        "created": 0,
        "model": model,
        "choices": [
            {
                "index": 0,
                "message": {"role": "assistant", "content": ANSWER},
                "finish_reason": "stop",
            }
        ],
        "usage": USAGE,
    }


def stream_chunks(model: str) -> list[dict]:
    base = {"id": "stub-1", "object": "chat.completion.chunk", "created": 0, "model": model}
    return [
        {**base, "choices": [{"index": 0, "delta": {"role": "assistant", "content": ANSWER}, "finish_reason": None}]},
        {**base, "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": USAGE},
    ]


class Handler(BaseHTTPRequestHandler):
    def log_message(self, format: str, *args: object) -> None:  # noqa: A002
        sys.stderr.write("stub-provider: " + (format % args) + "\n")

    def do_POST(self) -> None:  # noqa: N802
        length = int(self.headers.get("Content-Length") or 0)
        try:
            request = json.loads(self.rfile.read(length) or b"{}")
        except json.JSONDecodeError:
            request = {}
        model = str(request.get("model") or "stub")
        if request.get("stream"):
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Cache-Control", "no-cache")
            self.end_headers()
            for chunk in stream_chunks(model):
                self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode())
            self.wfile.write(b"data: [DONE]\n\n")
        else:
            payload = json.dumps(completion_body(model)).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

    def do_GET(self) -> None:  # noqa: N802
        payload = json.dumps({"object": "list", "data": [{"id": "stub", "object": "model"}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=0, help="loopback port (0 picks a free one)")
    parser.add_argument("--port-file", help="write the bound port here once listening")
    args = parser.parse_args()
    server = HTTPServer(("127.0.0.1", args.port), Handler)
    if args.port_file:
        with open(args.port_file, "w", encoding="utf-8") as handle:
            handle.write(str(server.server_address[1]))
    server.serve_forever()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
