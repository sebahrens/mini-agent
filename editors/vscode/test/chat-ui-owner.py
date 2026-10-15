#!/usr/bin/env python3
"""Own one isolated, signed-out VS Code UI fixture and loopback-only model."""
import argparse, hashlib, http.server, json, os, signal, socket, subprocess, threading, time, shutil
import urllib.request
from pathlib import Path
from chat_ui_platform import NativeProcesses, fixture_environment, install_command, launch_command, prepare_private_fixture

def scripted_response(payload, fixture):
 messages = payload.get('messages', [])
 content = next((m.get('content', '') for m in reversed(messages) if m.get('role') == 'user'), '')
 text = content if isinstance(content, str) else json.dumps(content)
 scenario = 'TOOL_RESULT' if messages and messages[-1].get('role') == 'tool' else next((s for s in ['PERMISSION_ALLOW', 'PERMISSION_DENY', 'PERMISSION_CANCEL'] if s in text), 'REPLY')
 def chunk(delta, finish):
  return {'id':'offline-ide-ui', 'object':'chat.completion.chunk', 'created':0, 'model':'offline-ide', 'choices':[{'index':0, 'delta':delta, 'finish_reason':finish}]}
 if scenario.startswith('PERMISSION_'):
  delta = {'role':'assistant', 'tool_calls':[{'index':0, 'id':'fixture-'+scenario, 'type':'function', 'function':{'name':'read', 'arguments':json.dumps({'path':str(fixture)})}}]}
  finish = 'tool_calls'
 else:
  delta = {'role':'assistant', 'content':'OFFLINE_AFTER_TOOL' if scenario == 'TOOL_RESULT' else 'OFFLINE_IDE_CHAT_RENDERED'}
  finish = 'stop'
 return scenario, 'data: '+json.dumps(chunk(delta,None))+'\n\ndata: '+json.dumps(chunk({},finish))+'\n\ndata: [DONE]\n\n'

def verify_provider_results(events):
 rows = [json.loads(line) for line in events.read_text().splitlines()]
 assert [row['scenario'] for row in rows] == ['REPLY','PERMISSION_ALLOW','TOOL_RESULT','PERMISSION_DENY','TOOL_RESULT','PERMISSION_CANCEL']
 def result(row, identity):
  return next(message['content'] for message in row['messages'] if message.get('role') == 'tool' and message.get('tool_call_id') == identity)
 assert 'OFFLINE_IDE_FIXTURE' in result(rows[2], 'fixture-PERMISSION_ALLOW')
 denied = result(rows[4], 'fixture-PERMISSION_DENY')
 assert denied == 'Permission denied by user' and 'OFFLINE_IDE_FIXTURE' not in denied
 assert not any(message.get('tool_call_id') == 'fixture-PERMISSION_CANCEL' for row in rows for message in row['messages'] if message.get('role') == 'tool')
 return len(rows)

def main():
 p=argparse.ArgumentParser();p.add_argument('--root',type=Path,required=True);p.add_argument('--app',type=Path,required=True);p.add_argument('--vsix',type=Path,required=True);p.add_argument('--binary',type=Path,required=True);p.add_argument('--exercise-ui',action='store_true');a=p.parse_args()
 root=a.root;root.mkdir(mode=0o700,parents=True,exist_ok=False)
 for name in ['workspace','profile/User','extensions','config','data','local-data','state','cache','credentials']:(root/name).mkdir(mode=0o700,parents=True)
 shutil.copytree(Path(__file__).parent/'chat-ui-provider',root/'extensions/mini-agent-tests.offline-chat-fixture-0.0.1')
 fixture=root/'workspace/fixture.txt';fixture.write_text('OFFLINE_IDE_FIXTURE\n')
 events=root/'provider-events.jsonl';events.touch()
 class Handler(http.server.BaseHTTPRequestHandler):
  def log_message(self,*args):pass
  def do_GET(self):
   raw=json.dumps({'data':[{'id':'offline-ide','object':'model','owned_by':'local-fixture'}]}).encode();self.send_response(200);self.send_header('Content-Type','application/json');self.end_headers();self.wfile.write(raw)
  def do_POST(self):
   size=int(self.headers.get('Content-Length','0'))
   if size>1024*1024:self.send_error(413);return
   payload=json.loads(self.rfile.read(size));messages=payload.get('messages',[])
   scenario, raw = scripted_response(payload, fixture)
   with events.open('a') as out:out.write(json.dumps({'path':self.path,'scenario':scenario,'messages':messages})+'\n')
   self.send_response(200);self.send_header('Content-Type','text/event-stream');self.end_headers()
   try:self.wfile.write(raw.encode())
   except (BrokenPipeError,ConnectionResetError):pass
 server=http.server.ThreadingHTTPServer(('127.0.0.1',0),Handler);server.daemon_threads=True
 thread=threading.Thread(target=server.serve_forever);port=server.server_port
 settings={'telemetry.telemetryLevel':'off','update.mode':'none','extensions.autoUpdate':False,'extensions.autoCheckUpdates':False,'workbench.startupEditor':'none','security.workspace.trust.enabled':False,'chat.disableAIFeatures':False,'chat.utilityModel':'mini-agent-offline-fixture/offline-ide','chat.utilitySmallModel':'mini-agent-offline-fixture/offline-ide','mini-agent.executablePath':str(a.binary)}
 (root/'profile/User/settings.json').write_text(json.dumps(settings,indent=2))
 (root/'profile/User/chatLanguageModels.json').write_text(json.dumps([{'name':'Owned Offline Fixture','vendor':'mini-agent-offline-fixture'},{'name':'Unused Custom Endpoint','vendor':'customendpoint','apiKey':'unused-local-fixture-key','apiType':'chat-completions','models':[{'id':'offline-ide','name':'Owned Offline Fixture','url':f'http://127.0.0.1:{port}/v1/chat/completions','toolCalling':True,'vision':False,'maxInputTokens':8192,'maxOutputTokens':256}]}],indent=2))
 (root/'config/config.toml').write_text(f'''provider = "offline-ide"
model = "offline-ide"
max_tokens = 256
session_title_model = "off"
default_permission_mode = "restrictive"
[custom_providers.offline-ide]
provider_type = "openai"
base_url = "http://127.0.0.1:{port}/v1"
api_key_env = "MINI_AGENT_IDE_FIXTURE_KEY"
api_style = "completions"
''');(root/'config/config.toml').chmod(0o600)
 private_initialization=prepare_private_fixture(root,os.environ)
 env=fixture_environment(os.environ)
 env['MINI_AGENT_CHAT_FIXTURE_EVENTS']=str(root/'fixture-model-events.jsonl')
 env.update({'ZS_CONFIG_DIR':str(root/'config'),'ZS_DATA_DIR':str(root/'data'),'ZS_LOCAL_DATA_DIR':str(root/'local-data'),'ZS_STATE_DIR':str(root/'state'),'ZS_CACHE_DIR':str(root/'cache'),'ZS_CREDENTIALS_DIR':str(root/'credentials'),'MINI_AGENT_IDE_FIXTURE_KEY':'unused-local-fixture-key'})
 common=['--user-data-dir',str(root/'profile'),'--extensions-dir',str(root/'extensions')]
 processes=NativeProcesses()
 children=[];forced=[];status={'root':str(root),'provider_port':port,'home_preserved':env.get('HOME')==os.environ.get('HOME'),'errors':[],'private_fixture_initialization':private_initialization}
 def run_owned(args,log,timeout):
  with log.open('wb') as output:
   c=processes.start(args,env,output);children.append(c)
   try:return c.wait(timeout=timeout)
   finally:
    if c.poll() is None:processes.stop(c,force=True);c.wait();forced.append(c.pid)
 try:
  thread.start()
  code=run_owned(install_command(a.app,[*common,'--install-extension',str(a.vsix),'--force'],env),root/'install.log',90)
  if code:raise RuntimeError(f'VSIX installation failed {code}')
  with socket.socket() as s:s.bind(('127.0.0.1',0));debug_port=s.getsockname()[1]
  log=(root/'host.log').open('wb')
  c=processes.start([*launch_command(a.app),*common,'--new-window','--skip-welcome','--skip-release-notes',f'--remote-debugging-port={debug_port}',str(root/'workspace')],env,log);children.append(c)
  status.update({'pid':c.pid,'debug_port':debug_port,'binary_sha256':hashlib.sha256(a.binary.read_bytes()).hexdigest(),'vsix_sha256':hashlib.sha256(a.vsix.read_bytes()).hexdigest()});(root/'host.json').write_text(json.dumps(status,indent=2));print(json.dumps(status),flush=True)
  if a.exercise_ui:
   deadline=time.monotonic()+30
   while True:
    if c.poll() is not None:raise RuntimeError('VS Code exited before workbench readiness')
    try:
     with urllib.request.urlopen(f'http://127.0.0.1:{debug_port}/json/list',timeout=1) as response:
      if any(target.get('type') == 'page' and 'workbench' in target.get('url','') for target in json.load(response)):break
    except (OSError,ValueError):pass
    if time.monotonic()>=deadline:raise RuntimeError('VS Code workbench did not become ready')
    time.sleep(.05)
   driver=Path(__file__).with_name('chat-ui-cdp.mjs')
   def ui(action,argument=''):
    with (root/'ui-actions.log').open('ab') as out:
     command=['node',str(driver),str(root),action,argument]
     result=subprocess.run(command,stdout=out,stderr=subprocess.STDOUT,timeout=15)
     if result.returncode:raise RuntimeError(f'UI action failed: {action} {argument}')
   def capture(name):
    ui('inspect');shutil.copyfile(root/'dom-latest.json',root/(name+'.json'));ui('screenshot',str(root/(name+'.png')))
   def submit(text):
    ui('click','textarea');ui('key','a,KeyA,4');ui('text','@mini-agent '+text);ui('key','Enter,Enter')
   ui('waitText','Owned Offline Fixture')
   submit('Reply to this offline Chat UI fixture.');ui('waitText','OFFLINE_IDE_CHAT_RENDERED');ui('waitIdle');capture('real-chat-rendered')
   submit('PERMISSION_ALLOW');ui('waitText','Allow once');capture('permission-allow-picker');ui('clickText','Allow once');ui('waitText','OFFLINE_AFTER_TOOL');ui('waitIdle');capture('permission-allow-rendered')
   submit('PERMISSION_DENY');ui('waitText','Allow once');capture('permission-deny-picker');ui('clickText','Deny (reject once)');ui('waitIdle');capture('permission-deny-rendered')
   submit('PERMISSION_CANCEL');ui('waitText','Allow once');capture('permission-cancel-pending');ui('click','[aria-label^="Cancel ("]');ui('waitIdle');capture('permission-cancel-idle')
   status['posts']=verify_provider_results(events)
   provider_events=[json.loads(line) for line in (root/'fixture-model-events.jsonl').read_text().splitlines()]
   assert not any(event['stage']=='fixture-model-response' for event in provider_events)
   assert any(event['stage']=='models-selected' and event['models'] for event in provider_events)
   status['ui_acceptance']={'rendered':True,'allow':True,'deny':True,'cancel_closes_picker_without_tool_result':True,'fixture_model_response_calls':0}
   ui('quit');c.wait(timeout=10)
   assert c.returncode==0
  else:
   deadline=time.monotonic()+480
   while c.poll() is None and not (root/'stop').exists() and time.monotonic()<deadline:time.sleep(.2)
  status['exit_before_cleanup']=c.poll()
 except BaseException as error:
  status['errors'].append(str(error))
  raise
 finally:
  for c in reversed(children):
   try:
    if c.poll() is None:
     processes.stop(c)
     try:c.wait(timeout=8)
     except subprocess.TimeoutExpired:processes.stop(c,force=True);c.wait();forced.append(c.pid)
   except BaseException as error:status['errors'].append('child cleanup: '+str(error))
  try:
   if thread.is_alive():server.shutdown()
   server.server_close()
   if thread.ident is not None:thread.join()
  finally:
   try:status['descendant_retirement']=processes.close()
   except BaseException as error:status['errors'].append('descendant cleanup: '+str(error))
  status['listeners_closed']={}
  for label,listener_port in [('provider',port),('cdp',status.get('debug_port'))]:
   if listener_port is not None:
    with socket.socket() as listener:status['listeners_closed'][label]=listener.connect_ex(('127.0.0.1',listener_port))!=0
  status['forced_cleanup']=forced
  status['child_exit_codes']=[c.returncode for c in children]
  (root/'retirement.json').write_text(json.dumps(status,indent=2))
  if status['errors'] or status.get('descendant_retirement',{}).get('forced_descendants',True) or not all(status['listeners_closed'].values()):
   raise RuntimeError('UI descendant/listener retirement was not clean; see retirement.json')
if __name__=='__main__':main()
