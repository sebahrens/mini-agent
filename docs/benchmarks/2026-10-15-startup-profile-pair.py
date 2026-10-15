#!/usr/bin/env python3
"""Offline same-source startup profile comparison; never changes HOME or drops OS caches."""
from pathlib import Path
import argparse, hashlib, json, os, platform, re, resource, signal, statistics, subprocess, time


def capture(command):
    return subprocess.check_output(command, text=True, timeout=15).strip()


def sha(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def environment(root):
    # Closed, nonsecret child environment. Preserve the operator's real HOME.
    allowed = ('HOME', 'USER', 'LOGNAME', 'TMPDIR', 'LANG', 'LC_ALL', '__CF_USER_TEXT_ENCODING')
    env = {key: os.environ[key] for key in allowed if key in os.environ}
    env.update(PATH='/usr/bin:/bin:/usr/sbin:/sbin', TERM='dumb', NO_COLOR='1', RUST_LOG='off',
               MINI_AGENT_HOME=str(root), MINI_AGENT_CONTAINMENT_EVIDENCE='1')
    for name, leaf in [('CONFIG', 'config'), ('DATA', 'data'), ('LOCAL_DATA', 'local-data'),
                       ('STATE', 'state'), ('CACHE', 'cache'), ('CREDENTIALS', 'credentials')]:
        env['ZS_' + name + '_DIR'] = str(root / leaf)
    return env


def provision(root):
    root.mkdir(mode=0o700)
    (root / 'workspace').mkdir(mode=0o700)
    return environment(root)


def owned_processes(base):
    # Do not publish unrelated process argv or any inherited credentials.
    rows = capture(['/bin/ps', '-axo', 'pid=,ppid=,command=']).splitlines()
    return [row.strip().split(None, 2)[:2] for row in rows if str(base) in row]


def invoke(binary, flag, case, env, base):
    started = time.perf_counter()
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    process = subprocess.Popen([str(binary), flag], cwd=case/'workspace', env=env,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
    try:
        stdout, stderr = process.communicate(timeout=30)
    except BaseException:
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            process.communicate(timeout=3)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.communicate(timeout=3)
        raise
    elapsed = time.perf_counter()-started
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    assert len(stdout) <= 2*1024*1024 and len(stderr) <= 256*1024
    assert process.returncode == 0, (flag, process.returncode, stderr.decode(errors='replace')[-2000:])
    text = stdout.decode('utf-8')
    evidence = re.findall(r'MACOS_SOURCE_DIGEST_EXPECTATION_V1=(source_read|matched_image|retained_snapshot)', stderr.decode('utf-8'))
    records = sorted(case.rglob('source-digest-v1.json'))
    if flag == '--print-config':
        assert 'not probed' in text
        assert not evidence and not records, 'no-model command must not prepare a worker image'
    else:
        assert text.strip() == 'JS runtime check: PASS (2)', text
        assert records and evidence, 'real contained-worker and persistent-cache evidence required'
    # Main returns only after its contained worker/guardian cleanup. Allow the
    # OS at most two seconds to report process retirement, without altering it.
    deadline = time.monotonic()+2
    while owned_processes(base):
        assert time.monotonic() < deadline, 'owned worker/guardian did not retire'
        time.sleep(.02)
    return {'wall_ms': elapsed*1000, 'user_ms': (after.ru_utime-before.ru_utime)*1000,
            'system_ms': (after.ru_stime-before.ru_stime)*1000, 'exit': process.returncode,
            'stdout_sha256': hashlib.sha256(stdout).hexdigest(), 'stdout_bytes': len(stdout),
            'source_digest_evidence': evidence, 'persistent_records': len(records),
            'owned_processes_after': 0}


def summarize(rows):
    values = sorted(row['wall_ms'] for row in rows)
    return {'n': len(rows), 'median_wall_ms': statistics.median(values),
            'min_wall_ms': values[0], 'max_wall_ms': values[-1],
            'p95_nearest_rank_wall_ms': values[max(0, __import__('math').ceil(.95*len(values))-1)],
            'median_user_ms': statistics.median(row['user_ms'] for row in rows),
            'median_system_ms': statistics.median(row['system_ms'] for row in rows)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--paths', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    paths = json.loads(args.paths.read_text())
    base = Path(paths['base'])
    source = Path(paths['source'])
    assert Path.cwd() == source, 'run from the pinned source checkout'
    assert capture(['git','diff','--name-only','c79475dd','HEAD']) == ''
    assert capture(['git','diff','--name-only']) == ''
    report = {'format':'mini-agent-startup-profile-pair/1', 'source': paths,
              'host': {'system':platform.platform(), 'machine':platform.machine(),
                       'cpu':capture(['/usr/sbin/sysctl','-n','machdep.cpu.brand_string']),
                       'logical_cpus':os.cpu_count(), 'rustc':capture(['rustc','-vV'])},
              'features':'Cargo default features (same for both profiles)',
              'release_exception':'One explicit user-approved isolated release-profile benchmark; no production optimization/settings changes',
              'method': {'cold':'Fresh private application roots and no expected-digest record; OS filesystem/dyld caches are not flushed',
                         'warm':'Independent new process using the same private application roots after one successful seed',
                         'sampling':'5 cold application states and 10 warm processes per command/profile; serial after both builds',
                         'scope':'Offline --print-config and real --js-runtime-check arithmetic only; no model/provider/account calls',
                         'home':'Actual HOME preserved; all application roots explicitly isolated'}, 'profiles':{}}
    for profile in ['debug','release']:
        binary = Path(paths[profile+'_root'])/'bin/mini-agent'
        version = capture([str(binary),'--version'])
        assert re.search(r'^build: [0-9a-f]{64}$',version,re.M), version
        item={'binary':str(binary),'sha256':sha(binary),'size_bytes':binary.stat().st_size,
              'version':version,'commands':{}}
        for name,flag in [('print_config','--print-config'),('js_runtime_check','--js-runtime-check')]:
            cold=[]
            for index in range(5):
                case=base/f'{profile}-{name}-cold-{index}'
                row=invoke(binary,flag,case,provision(case),base)
                if name=='js_runtime_check': assert 'source_read' in row['source_digest_evidence']
                cold.append(row)
            # Keep the last fresh successful state to seed the warm process set.
            warm=[]
            for index in range(10):
                row=invoke(binary,flag,case,environment(case),base)
                if name=='js_runtime_check': assert 'matched_image' in row['source_digest_evidence']
                warm.append(row)
            item['commands'][name]={'cold':{'summary':summarize(cold),'samples':cold},
                                    'warm':{'summary':summarize(warm),'samples':warm}}
        report['profiles'][profile]=item
        args.output.write_text(json.dumps(report,indent=2)+'\n')
    report['driver_sha256']=sha(Path(__file__))
    report['all_owned_processes_retired']=not owned_processes(base)
    assert report['all_owned_processes_retired']
    args.output.write_text(json.dumps(report,indent=2)+'\n')
    print(json.dumps({profile:{command:values['warm']['summary'] for command,values in data['commands'].items()}
                      for profile,data in report['profiles'].items()},indent=2))

if __name__=='__main__':main()
