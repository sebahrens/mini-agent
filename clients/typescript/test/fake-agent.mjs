// A stand-in speaking the control protocol: on a user line it asks can_use_tool, then
// emits tool_result and result; `interrupt` and `initialize` are acknowledged.
import { createInterface } from 'node:readline';
const out = (v) => process.stdout.write(`${JSON.stringify(v)}\n`);
let turnAsk = null;
let turnText = '';
createInterface({ input: process.stdin }).on('line', (line) => {
  const msg = JSON.parse(line);
  if (msg.type === 'user') {
    turnText = msg.message.content;
    out({ type: 'system', subtype: 'init', session_id: 's-1' });
    if (turnText === 'wait') return;
    turnAsk = 'perm_1';
    out({ type: 'control_request', request_id: 'perm_1', request: { subtype: 'can_use_tool', tool_name: 'shell', input: 'ls', input_bytes: 2, truncated: false, tool_input: { command: 'ls' } } });
  } else if (msg.type === 'control_response' && msg.response.request_id === turnAsk) {
    const allowed = msg.response.response.behavior === 'allow';
    out({ type: 'tool_result', id: 't1', output: allowed ? 'file' : 'denied', is_error: !allowed });
    out({ type: 'result', is_error: false, result: allowed ? 'allowed' : 'denied' });
  } else if (msg.type === 'control_request') {
    const id = msg.request_id;
    const sub = msg.request.subtype;
    if (sub === 'interrupt') {
      out({ type: 'control_response', response: { subtype: 'success', request_id: id } });
      out({ type: 'result', is_error: true, result: 'interrupted' });
    } else if (sub === 'initialize') {
      out({ type: 'control_response', response: { subtype: 'success', request_id: id, response: { protocol: 'mini-agent-control/1' } } });
    } else {
      out({ type: 'control_response', response: { subtype: 'error', request_id: id, error: 'unknown' } });
    }
  }
});
process.stdin.on('end', () => process.exit(0));
