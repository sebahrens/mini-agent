"""Cheap deterministic checks for the native UI fixture's wire assertions."""
import copy
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('chat_ui_owner', Path(__file__).with_name('chat-ui-owner.py'))
owner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(owner)

class FixtureTests(unittest.TestCase):
    def test_permission_requests_use_only_owned_literal_file_and_closed_scenarios(self):
        for scenario in ['PERMISSION_ALLOW', 'PERMISSION_DENY', 'PERMISSION_CANCEL']:
            with self.subTest(scenario=scenario):
                selected, wire = owner.scripted_response({'messages':[{'role':'user','content':scenario}]}, Path('/owned fixture/file.txt'))
                self.assertEqual(selected, scenario)
                chunks = [json.loads(line[6:]) for line in wire.splitlines() if line.startswith('data: ') and line != 'data: [DONE]']
                call = chunks[0]['choices'][0]['delta']['tool_calls'][0]
                self.assertEqual(call['id'], 'fixture-' + scenario)
                self.assertEqual(call['function']['name'], 'read')
                self.assertEqual(json.loads(call['function']['arguments']), {'path':'/owned fixture/file.txt'})
                self.assertEqual(chunks[-1]['choices'][0]['finish_reason'], 'tool_calls')
                self.assertTrue(wire.endswith('data: [DONE]\n\n'))

    def test_actual_tool_result_wins_over_earlier_permission_prompt(self):
        selected, wire = owner.scripted_response({'messages':[{'role':'user','content':'PERMISSION_ALLOW'},{'role':'tool','content':'result'}]}, Path('/owned/file'))
        self.assertEqual(selected, 'TOOL_RESULT')
        self.assertIn('OFFLINE_AFTER_TOOL', wire)
        self.assertNotIn('tool_calls', wire)

    def test_acceptance_refuses_wrong_denial_or_cancelled_tool_result(self):
        rows = [{'scenario':s,'messages':[]} for s in ['REPLY','PERMISSION_ALLOW','TOOL_RESULT','PERMISSION_DENY','TOOL_RESULT','PERMISSION_CANCEL']]
        rows[2]['messages'] = [{'role':'tool','tool_call_id':'fixture-PERMISSION_ALLOW','content':'OFFLINE_IDE_FIXTURE'}]
        rows[4]['messages'] = [{'role':'tool','tool_call_id':'fixture-PERMISSION_DENY','content':'Permission denied by user'}]
        with tempfile.TemporaryDirectory(prefix='mini-agent-chat-ui-unit-') as root:
            path = Path(root) / 'events.jsonl'
            def check(candidate):
                path.write_text(''.join(json.dumps(row)+'\n' for row in candidate))
                return owner.verify_provider_results(path)
            self.assertEqual(check(rows), 6)
            leaked = copy.deepcopy(rows)
            leaked[4]['messages'][0]['content'] = 'OFFLINE_IDE_FIXTURE'
            with self.assertRaises(AssertionError): check(leaked)
            cancelled = copy.deepcopy(rows)
            cancelled[5]['messages'].append({'role':'tool','tool_call_id':'fixture-PERMISSION_CANCEL','content':'anything'})
            with self.assertRaises(AssertionError): check(cancelled)

if __name__ == '__main__': unittest.main()
