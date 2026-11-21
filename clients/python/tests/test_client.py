import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
from mini_agent_client import ControlError, Session, build_args  # noqa: E402

FAKE = os.path.join(os.path.dirname(__file__), "fake_agent.py")


def fake(**kw):
    return Session(sys.executable, argv=[FAKE], **kw)


class ClientTests(unittest.TestCase):
    def test_build_args(self):
        args = build_args(prompt="hi", ask_timeout_secs=5, model="fast")
        self.assertEqual(args[:2], ["-p", "hi"])
        for flag in ["--input-format", "--stream-json-control", "--stream-json-schema", "--stream-json-ask-timeout", "--model"]:
            self.assertIn(flag, args)

    def test_turn_with_permission_callback(self):
        seen = []

        def allow(name, tool_input, ask):
            seen.append((name, tool_input))
            return {"behavior": "allow"}

        with fake(can_use_tool=allow) as session:
            session.send("run ls")
            records = list(session.turn())
        self.assertEqual(seen, [("shell", {"command": "ls"})])
        self.assertEqual(records[-1]["result"], "allowed")
        self.assertFalse(any(r["type"] == "control_request" for r in records))

    def test_deny_default_interrupt_and_errors(self):
        with fake() as session:
            session.send("run ls")
            self.assertEqual(list(session.turn())[-1]["result"], "denied")
            session.send("wait")
            self.assertEqual(session.interrupt(), {})
            self.assertEqual(list(session.turn())[-1]["result"], "interrupted")
            self.assertEqual(session.initialize(), {"protocol": "mini-agent-control/1"})
            with self.assertRaises(ControlError):
                session.set_model("x")

    def test_callback_exception_denies(self):
        def boom(*_):
            raise RuntimeError("no")

        with fake(can_use_tool=boom) as session:
            session.send("run ls")
            self.assertEqual(list(session.turn())[-1]["result"], "denied")

    def test_requests_fail_after_exit(self):
        session = fake()
        session.close()
        with self.assertRaises(ControlError):
            session.interrupt()
        self.assertEqual(list(session.messages()), [])


if __name__ == "__main__":
    unittest.main()
