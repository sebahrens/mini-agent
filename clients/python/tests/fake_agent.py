"""Stand-in speaking the control protocol, mirroring clients/typescript/test/fake-agent.mjs."""
import json
import sys

ask = None


def out(value):
    sys.stdout.write(json.dumps(value) + "\n")
    sys.stdout.flush()


for line in sys.stdin:
    msg = json.loads(line)
    kind = msg.get("type")
    if kind == "user":
        text = msg["message"]["content"]
        out({"type": "system", "subtype": "init", "session_id": "s-1"})
        if text == "wait":
            continue
        ask = "perm_1"
        out({"type": "control_request", "request_id": "perm_1", "request": {
            "subtype": "can_use_tool", "tool_name": "shell", "input": "ls",
            "input_bytes": 2, "truncated": False, "tool_input": {"command": "ls"}}})
    elif kind == "control_response" and msg["response"]["request_id"] == ask:
        allowed = msg["response"]["response"]["behavior"] == "allow"
        out({"type": "tool_result", "id": "t1", "output": "file" if allowed else "denied", "is_error": not allowed})
        out({"type": "result", "is_error": False, "result": "allowed" if allowed else "denied"})
    elif kind == "control_request":
        rid, sub = msg["request_id"], msg["request"]["subtype"]
        if sub == "interrupt":
            out({"type": "control_response", "response": {"subtype": "success", "request_id": rid}})
            out({"type": "result", "is_error": True, "result": "interrupted"})
        elif sub == "initialize":
            out({"type": "control_response", "response": {"subtype": "success", "request_id": rid, "response": {"protocol": "mini-agent-control/1"}}})
        else:
            out({"type": "control_response", "response": {"subtype": "error", "request_id": rid, "error": "unknown"}})
