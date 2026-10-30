#!/usr/bin/env python3
"""Owned macOS PTY regression for terminal EOF (no model request or account).

Run --mode quit for the normal key/paste/quit control, then the default hangup
mode. --expect-hang is only for recording an affected baseline. Each child owns
its PTY/session, and finally closes and reaps that exact process group before
removing fixture files. Reports are retained under /private/tmp/mini-agent-118-LABEL.
"""
import argparse,errno,fcntl,hashlib,json,os,pty,resource,select,signal,socket,struct,subprocess,tempfile,termios,time
from pathlib import Path
ap=argparse.ArgumentParser();ap.add_argument('--binary',required=True);ap.add_argument('--label',required=True);ap.add_argument('--mode',choices=['hangup','quit'],default='hangup');ap.add_argument('--expect-hang',action='store_true');a=ap.parse_args();binary=Path(a.binary).resolve()
out=Path('/private/tmp/mini-agent-118-'+a.label);out.mkdir(exist_ok=True)
def stop_driver(signum, frame): raise KeyboardInterrupt('driver termination requested')
signal.signal(signal.SIGTERM,stop_driver)
child=None;master=None;slave=None;listener=None;painted=bytearray();provider_requests=[];report={'binary':str(binary),'sha256':hashlib.sha256(binary.read_bytes()).hexdigest(),'mode':a.mode,'forced_cleanup':False}
with tempfile.TemporaryDirectory(prefix='mini-agent-118-owned-',dir='/private/tmp') as temp:
 root=Path(temp);work=root/'workspace';work.mkdir();env=os.environ.copy()
 for key in tuple(env):
  if key.startswith(('ZS_','OPENAI_','ANTHROPIC_','OPENROUTER_')) or key.endswith(('_API_KEY','_AUTH_TOKEN')):env.pop(key,None)
 for key in ('MINI_AGENT_CONFIG_DIR','MINI_AGENT_DATA_DIR','MINI_AGENT_LOCAL_DATA_DIR','MINI_AGENT_STATE_DIR','MINI_AGENT_CACHE_DIR','MINI_AGENT_CREDENTIALS_DIR'):
  p=root/key;p.mkdir(mode=0o700);env[key]=str(p)
 env.update(TERM='xterm-256color',RUST_LOG='off',OPENAI_API_KEY='owned-local118-only')
 listener=socket.socket();listener.bind(('127.0.0.1',0));listener.listen();port=listener.getsockname()[1]
 config=Path(env['MINI_AGENT_CONFIG_DIR'])/'config.toml';config.write_text('provider="terminal118"\nmodel="fixture"\nsession_title_model="off"\nauto_update_prompts=false\nauto_update_themes=false\n[lsp]\nenabled=false\n[custom_providers.terminal118]\nprovider_type="openai"\napi_style="completions"\napi_key_env="OPENAI_API_KEY"\nbase_url="http://127.0.0.1:'+str(port)+'/v1"\n');config.chmod(0o600)
 answered=False
 def setup():
  os.setsid();fcntl.ioctl(0,termios.TIOCSCTTY,0);resource.setrlimit(resource.RLIMIT_STACK,(1048576,resource.getrlimit(resource.RLIMIT_STACK)[1]))
 def drain():
  global answered
  if listener is not None and select.select([listener],[],[],0)[0]:
   with listener.accept()[0] as connection:
    connection.settimeout(1);header=b''
    while not header.endswith(b'\r\n\r\n'):
     assert len(header)<8192;part=connection.recv(1)
     if not part:break
     header+=part
    first=header.split(b'\r\n',1)[0].decode();provider_requests.append(first)
    body=json.dumps({'object':'list','data':[{'id':'fixture','object':'model','context_length':100000}]}).encode()
    connection.sendall(b'HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: '+str(len(body)).encode()+b'\r\n\r\n'+body)
  if master is not None and select.select([master],[],[],.05)[0]:
   try:painted.extend(os.read(master,65536))
   except OSError as error:
    if error.errno!=errno.EIO:raise
  if master is not None and b'\x1b]11;?\x07' in painted and not answered:
   os.write(master,b'\x1b]11;rgb:0000/0000/0000\x07');answered=True
 try:
  master,slave=pty.openpty();fcntl.ioctl(slave,termios.TIOCSWINSZ,struct.pack('HHHH',42,190,0,0))
  child=subprocess.Popen([str(binary),'--provider','terminal118','--model','fixture','--no-session','--no-context-files','--no-sandbox','--tools','read','--load-prompt','code'],cwd=work,env=env,stdin=slave,stdout=slave,stderr=slave,preexec_fn=setup)
  report.update(pid=child.pid,pgid=os.getpgid(child.pid),sid=os.getsid(child.pid));assert report['pgid']==report['sid']==child.pid
  deadline=time.monotonic()+20
  while b'code' not in painted:
   assert child.poll() is None,('early exit',child.returncode,bytes(painted)[-1800:]);assert time.monotonic()<deadline,('UI ready',bytes(painted)[-1800:]);drain()
  deadline=time.monotonic()+15
  while termios.tcgetattr(slave)[3] & (termios.ECHO|termios.ICANON):
   assert child.poll() is None;assert time.monotonic()<deadline;drain()
  assert child.poll() is None
  report['raw_mode_observed']=True
  os.write(master,b'\x1b[200~HANGUP118_READER_READY\x1b[201~');deadline=time.monotonic()+15
  while b'HANGUP118_READER_READY' not in painted:
   assert child.poll() is None,('early reader exit',child.returncode);assert time.monotonic()<deadline,('reader did not echo input',bytes(painted)[-1800:]);drain()
  report['reader_echo_observed']=True
  os.close(slave);slave=None
  if a.mode=='quit':
   os.write(master,b'\x03');
   until=time.monotonic()+.2
   while time.monotonic()<until:drain()
   os.write(master,b'\x1b[200~owned paste118\nsecond line\x1b[201~');
   deadline=time.monotonic()+10
   while b'owned paste118' not in painted or b'second line' not in painted:
    assert child.poll() is None;assert time.monotonic()<deadline,('paste echo',bytes(painted)[-1800:]);drain()
   os.write(master,b'\x03');
   until=time.monotonic()+.2
   while time.monotonic()<until:drain()
   os.write(master,b'/quit');
   until=time.monotonic()+.2
   while time.monotonic()<until:drain()
   os.write(master,b'\r');deadline=time.monotonic()+10
   while child.poll() is None and time.monotonic()<deadline:drain()
   assert child.poll()==0,('normal input/paste/quit',child.poll(),bytes(painted)[-1800:]);report['exit']=child.returncode;report['paste_preserved']=True
  else:
   os.close(master);master=None;started=time.monotonic()
   try:report['exit']=child.wait(timeout=4);report['hung']=False
   except subprocess.TimeoutExpired:
    report['hung']=True
    report['cpu']=subprocess.run(['/bin/ps','-p',str(child.pid),'-o','pcpu=,time='],capture_output=True,text=True,timeout=3).stdout.strip()
    sample=subprocess.run(['/usr/bin/sample',str(child.pid),'1','10'],capture_output=True,text=True,timeout=8)
    (out/'sample.txt').write_text(sample.stdout+sample.stderr)
   report['elapsed_after_hangup']=time.monotonic()-started
   assert report['hung']==a.expect_hang,report
   if not report['hung']:assert report['exit'] in (0,1),report
 finally:
  signal.signal(signal.SIGTERM,signal.SIG_IGN);signal.signal(signal.SIGINT,signal.SIG_IGN)
  if slave is not None:os.close(slave)
  if master is not None:os.close(master)
  if child is not None:
   if child.poll() is None:
    report['forced_cleanup']=True
    try:os.killpg(child.pid,signal.SIGTERM)
    except ProcessLookupError:pass
    try:child.wait(timeout=1)
    except subprocess.TimeoutExpired:
     try:os.killpg(child.pid,signal.SIGKILL)
     except ProcessLookupError:pass
     child.wait(timeout=5)
   report['reaped_exit']=child.returncode;assert child.poll() is not None
  if listener is not None:
   report['unserved_provider_connections']=int(bool(select.select([listener],[],[],0)[0]));listener.close()
  report['provider_requests']=provider_requests
  report['no_model_requests']=all(request.startswith('GET ') for request in provider_requests)
  (out/'terminal.bin').write_bytes(painted);(out/'report.json').write_text(json.dumps(report,indent=2)+'\n')
assert not Path(temp).exists()
assert report['no_model_requests'],report
print(json.dumps(report,indent=2))
