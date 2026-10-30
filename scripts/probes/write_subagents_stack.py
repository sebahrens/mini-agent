#!/usr/bin/env python3
"""Exercise opt-in child edits through an owned local provider at a 1 MiB stack.

Run on Unix from the repository root with --binary pointing at a debug installation.
The fixture never sends requests to an external provider. It retains its owned
workspace, request bodies, and child output for diagnosing failed assertions.
"""
import argparse
import hashlib
import http.server
import json
import os
import pathlib
import resource
import signal
import subprocess
import tempfile
import threading

parser = argparse.ArgumentParser()
parser.add_argument('--binary', required=True)
args = parser.parse_args()
binary = pathlib.Path(args.binary).resolve()
root = pathlib.Path(tempfile.mkdtemp(prefix='mini-agent-write55-installed-')).resolve()
print(f'Owned fixture artifacts: {root}', flush=True)
home = root / 'home'
home.mkdir(mode=0o700)
workspace = root / 'workspace'
workspace.mkdir()
(home / 'agents').mkdir(mode=0o700)
(home / 'agents' / 'implementer.md').write_text('---\nmode: write\ntools: Read, Edit, Write, Shell\n---\nImplement exactly the delegated file edit. Report verified edits and missing checks.\n')
requests = []
errors = []
phase = {'allow': False}
stop = threading.Event()

class Handler(http.server.BaseHTTPRequestHandler):

    def log_message(self, *items):
        pass

    def do_POST(self):
        try:
            self.connection.settimeout(5)
            length = int(self.headers['Content-Length'])
            assert 0 < length < 1024 * 1024
            body = json.loads(self.rfile.read(length))
            i = len(requests)
            requests.append(body)
            names = {tool['function']['name'] for tool in body.get('tools', [])}

            def tool(name, arguments):
                return {'role': 'assistant', 'tool_calls': [{'index': 0, 'id': f'write55-{i}', 'type': 'function', 'function': {'name': name, 'arguments': json.dumps(arguments)}}]}
            if i == 0:
                assert 'task' in names
                delta = tool('task', {'prompts': ['Replace before with after in first.txt.', 'Replace before with after in second.txt.'], 'agent_type': 'implementer'})
                finish = 'tool_calls'
            elif not phase['allow']:
                assert i == 1 and 'task' in names and ('allow_write = true' in json.dumps(body))
                assert all(((workspace / name).read_text() == 'before\n' for name in ['first.txt', 'second.txt']))
                delta = {'role': 'assistant', 'content': 'WRITE55_REFUSED'}
                finish = 'stop'
            elif i == 7:
                assert 'task' in names and all(((workspace / name).read_text() == 'after\n' for name in ['first.txt', 'second.txt']))
                assert '[subagent output begins]' in json.dumps(body)
                delta = {'role': 'assistant', 'content': 'WRITE55_DONE'}
                finish = 'stop'
            else:
                assert names == {'read', 'edit', 'write', 'shell'}, names
                child = (i - 1) // 3
                step = (i - 1) % 3
                name = ['first.txt', 'second.txt'][child]
                assert name in json.dumps(body['messages'])
                if child == 1:
                    assert (workspace / 'first.txt').read_text() == 'after\n'
                if step == 0:
                    delta = tool('read', {'path': name})
                    finish = 'tool_calls'
                elif step == 1:
                    delta = tool('edit', {'path': name, 'block': '<<<<<<< SEARCH\nbefore\n=======\nafter\n>>>>>>> REPLACE'})
                    finish = 'tool_calls'
                else:
                    assert (workspace / name).read_text() == 'after\n'
                    delta = {'role': 'assistant', 'content': f'## Findings\n- [confidence: high] Edited {name}.\n\n## Unverified\n- None.\n\n## Coverage\n- Covered: {name} read and edit.\n- Skipped: None.'}
                    finish = 'stop'

            def chunk(delta, finish=None, usage=None):
                value = {'id': 'write55-fixture', 'object': 'chat.completion.chunk', 'created': 0, 'model': 'gpt-4.1', 'choices': [{'index': 0, 'delta': delta, 'finish_reason': finish}]}
                if usage:
                    value['usage'] = usage
                return ('data: ' + json.dumps(value) + '\n\n').encode()
            payload = chunk(delta) + chunk({}, finish, {'prompt_tokens': 20, 'completion_tokens': 5, 'total_tokens': 25}) + b'data: [DONE]\n\n'
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream')
            self.send_header('Content-Length', str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
        except Exception as error:
            errors.append(repr(error))
            raise
server = http.server.HTTPServer(('127.0.0.1', 0), Handler)
server.timeout = 0.2

def serve():
    while not stop.is_set():
        server.handle_request()
thread = threading.Thread(target=serve)
thread.start()
env = os.environ.copy()
env.update(HOME=str(home), MINI_AGENT_HOME=str(home), OPENAI_API_KEY='owned-local-write55', NO_PROXY='127.0.0.1', no_proxy='127.0.0.1', RUST_LOG='off')
for key in ['MINI_AGENT_CONFIG_DIR', 'MINI_AGENT_DATA_DIR', 'MINI_AGENT_LOCAL_DATA_DIR', 'MINI_AGENT_STATE_DIR', 'MINI_AGENT_CACHE_DIR', 'MINI_AGENT_CREDENTIALS_DIR']:
    env[key] = str(home)
for key in ['OPENROUTER_API_KEY', 'OPENROUTER_MODEL', 'ANTHROPIC_API_KEY', 'BASH_ENV', 'ENV']:
    env.pop(key, None)

def setup():
    os.setsid()
    resource.setrlimit(resource.RLIMIT_STACK, (1048576, resource.getrlimit(resource.RLIMIT_STACK)[1]))
reports = []
try:
    for allow in [False, True]:
        phase['allow'] = allow
        requests.clear()
        for name in ['first.txt', 'second.txt']:
            (workspace / name).write_text('before\n')
        (home / 'config.toml').write_text(
            f'''provider="write55-wire"
model="gpt-4.1"
max_tokens=1024
session_title_model="off"
auto_update_prompts=false
auto_update_themes=false
task_max_concurrency=4
[custom_providers.write55-wire]
provider_type="openai"
base_url="http://127.0.0.1:{server.server_port}/v1"
api_key_env="OPENAI_API_KEY"
api_style="completions"
[permission]
"*"="allow"
[subagents]
allow_write={str(allow).lower()}
[lsp]
enabled=false
'''
        )
        (home / 'config.toml').chmod(0o600)
        child = subprocess.Popen([str(binary), '--no-context-files', '--no-sandbox', '--no-session', '--tools', 'read,edit,write,shell,task', '--load-prompt', 'code', '-p', 'Delegate both file edits to implementer.', '--output', 'stream-json'], cwd=workspace, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, preexec_fn=setup)
        try:
            out, err = child.communicate(timeout=35)
        finally:
            if child.poll() is None:
                os.killpg(child.pid, signal.SIGTERM)
                try:
                    child.communicate(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(child.pid, signal.SIGKILL)
                    child.communicate(timeout=5)
        label = 'allowed' if allow else 'refused'
        (root / f'{label}.stdout').write_bytes(out)
        (root / f'{label}.stderr').write_bytes(err)
        (root / f'{label}.requests.json').write_text(json.dumps(requests, indent=2))
        assert child.returncode == 0, (label, child.returncode, err, out[-2000:])
        assert not errors, errors
        marker = b'WRITE55_DONE' if allow else b'WRITE55_REFUSED'
        assert marker in out, (label, out[-2000:])
        assert len(requests) == (8 if allow else 2), len(requests)
        reports.append({'allow_write': allow, 'posts': len(requests), 'exit': child.returncode, 'files': {name: (workspace / name).read_text() for name in ['first.txt', 'second.txt']}})
    report = {'passed': True, 'binary': str(binary), 'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(), 'stack_bytes': 1048576, 'external_provider_calls': 0, 'configured_concurrency': 4, 'write_children_serialized': True, 'runs': reports}
    (root / 'report.json').write_text(json.dumps(report, indent=2))
    print(root / 'report.json')
finally:
    stop.set()
    thread.join(6)
    server.server_close()
    assert not thread.is_alive()
