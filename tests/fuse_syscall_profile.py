#!/usr/bin/env python3
"""Bounded late-attach FUSE diagnostic. No host/perf/ptrace policy changes."""
import argparse, ctypes, hashlib, json, os, resource, shutil, signal
import subprocess, sys, time
from pathlib import Path

HEAD='5f7abb436c28e9851c2cae192e4126fceb7d3d19'

def save(path,value):
    temporary=path.with_name(path.name+'.new')
    temporary.write_text(json.dumps(value)+'\n');temporary.replace(path)

def proc(pid):
    text=Path(f'/proc/{pid}/stat').read_text().rsplit(')',1)[1].split()
    hz=os.sysconf('SC_CLK_TCK')
    return dict(user_s=int(text[11])/hz,system_s=int(text[12])/hz,
                reaped_child_user_s=int(text[13])/hz,
                reaped_child_system_s=int(text[14])/hz,threads=int(text[17]))

def tracer(pid):
    try:
        return int(next(s.split(':')[1] for s in Path(f'/proc/{pid}/status').read_text().splitlines() if s.startswith('TracerPid:')))
    except (FileNotFoundError,StopIteration):
        return -1

def payload(c):
    d=Path(c['case']);ready=d/'ready.json'
    cwd=os.getcwd()
    def unescape(s):
        for a,b in (('\\040',' '),('\\011','\t'),('\\012','\n'),('\\134','\\')):s=s.replace(a,b)
        return s
    fuse=any(unescape(line.split()[4])==cwd and line.split(' - ',1)[1].startswith('fuse')
             for line in Path('/proc/self/mountinfo').read_text().splitlines())
    assert fuse or c['allow_materialized'],'expected an actual FUSE mount'
    save(ready,dict(pid=os.getpid(),cwd=cwd,fuse=fuse))
    def wait_for(name):
        until=time.monotonic()+45
        while not (d/name).exists():
            if time.monotonic()>until:raise TimeoutError(name)
            time.sleep(.005)
    if c['trace']:wait_for('go')
    if (d/'abort').exists():return 125
    phases=[]
    for label in ('first','repeat'):
        before=resource.getrusage(resource.RUSAGE_SELF);begin=time.monotonic_ns()
        for i in range(c['files']):
            name=f'f{i:05d}';assert os.stat(name).st_size==c['size']
            with open(name,'rb') as f:data=f.read()
            expected=i.to_bytes(8,'little')+hashlib.shake_256(f'brick-fuse:{i}'.encode()).digest(c['size']-8)
            assert data==expected
        after=resource.getrusage(resource.RUSAGE_SELF)
        phases.append(dict(phase=label,wall_ns=time.monotonic_ns()-begin,
            user_s=after.ru_utime-before.ru_utime,system_s=after.ru_stime-before.ru_stime,
            files=c['files'],payload_bytes=c['files']*c['size']))
    usage=resource.getrusage(resource.RUSAGE_SELF)
    save(d/'done.json',dict(phases=phases,child_user_s=usage.ru_utime,
                           child_system_s=usage.ru_stime,fuse=fuse))
    if c['trace']:wait_for('finish')
    return 0

def launcher(c,script):
    d=Path(c['case'])
    env={k:v for k,v in os.environ.items() if not k.startswith(('GIT_','SSH_','LD_PRELOAD'))}
    env.update(TMPDIR=c['temporary'],LC_ALL='C',GIT_CONFIG_NOSYSTEM='1',GIT_CONFIG_GLOBAL=os.devnull)
    with (d/'stdout.log').open('wb') as out,(d/'stderr.log').open('wb') as err:
        policy='all' if c['allow_materialized'] else 'none'
        p=subprocess.Popen([c['binary'],'HEAD','--write-policy='+policy,'--',
            sys.executable,'-I','-S',str(script),'--payload',str(d/'config.json')],
            cwd=c['repo'],env=env,stdout=out,stderr=err,start_new_session=True)
    save(d/'server.json',dict(pid=p.pid))
    if not c['trace']:os._exit(0) # the supervisor reaps the reparented server
    until=time.monotonic()+15
    while not (d/'ready.json').exists():
        if p.poll() is not None:raise RuntimeError('server failed before child barrier')
        if time.monotonic()>until:raise TimeoutError('mount/child readiness')
        time.sleep(.005)
    child=json.loads((d/'ready.json').read_text())['pid']
    # This exact process is still an ancestor of the server and payload.
    # Attach AFTER fusermount3 completed; no PR_SET_PTRACER / Yama changes.
    argv=['strace','-f','-qq','-c','-o',str(d/'syscalls.txt')]
    if c['trace']=='requester':argv+=['-P',f'/proc/{child}/status']
    argv+=['-p',str(child if c['trace']=='command' else p.pid)]
    os.execvp(argv[0],argv)

def run_case(c,script):
    d=Path(c['case']);d.mkdir();save(d/'config.json',c)
    begin=time.monotonic_ns();own=resource.getrusage(resource.RUSAGE_SELF)
    launcher_pid=os.fork()
    if launcher_pid==0:
        try:launcher(c,script)
        except BaseException as e:
            (d/'launcher-error.txt').write_text(repr(e));os._exit(125)
    reaped={}
    def collect():
        while True:
            try:pid,status,u=os.wait4(-1,os.WNOHANG)
            except ChildProcessError:return
            if not pid:return
            reaped[pid]=dict(exit=os.waitstatus_to_exitcode(status),user_s=u.ru_utime,
                system_s=u.ru_stime,max_rss_kib=u.ru_maxrss,
                input_bytes=u.ru_inblock*512,output_bytes=u.ru_oublock*512)
    def until(test,seconds):
        end=time.monotonic()+seconds
        while not test():
            collect()
            if launcher_pid in reaped and c['trace']:
                raise RuntimeError('tracer exited before completion; inspect syscalls/launcher-error')
            if time.monotonic()>end:raise TimeoutError('profile barrier')
            time.sleep(.005)
    server=None;target=None
    try:
        until(lambda:(d/'server.json').exists(),15)
        server=json.loads((d/'server.json').read_text())['pid']
        if c['trace']:
            until(lambda:(d/'ready.json').exists(),15)
            child=json.loads((d/'ready.json').read_text())['pid']
            target=child if c['trace']=='command' else server
            until(lambda:all(tracer(int(t.name))==launcher_pid for t in Path(f'/proc/{target}/task').iterdir()),5)
            before=proc(server);save(d/'server-before.json',before)
            (d/'go').touch()
            until(lambda:(d/'done.json').exists(),30)
            after=proc(server);save(d/'server-after.json',after)
            # Detach BEFORE letting the command exit / FUSE unmount begin.
            os.kill(launcher_pid,signal.SIGINT)
            deadline=time.monotonic()+5
            while tracer(target)>0:
                if time.monotonic()>deadline:raise TimeoutError('tracer detach')
                time.sleep(.005)
            (d/'finish').touch()
        deadline=time.monotonic()+45
        while server not in reaped:
            collect()
            if time.monotonic()>deadline:raise TimeoutError('server cleanup')
            time.sleep(.005)
        collect()
        assert reaped[server]['exit']==0,reaped
        completed=json.loads((d/'done.json').read_text())
        assert len(completed['phases'])==2
        now=resource.getrusage(resource.RUSAGE_SELF)
        row=dict(case=d.name,traced=bool(c['trace']),trace_scope=c['trace'],
            elapsed_ns=time.monotonic_ns()-begin,server_and_waited_children=reaped[server],
            separately_reaped_tracer=reaped.get(launcher_pid),child=completed,
            supervisor_user_s=now.ru_utime-own.ru_utime,
            supervisor_system_s=now.ru_stime-own.ru_stime)
        if c['trace']:
            row['server_work_own_cpu_tick_delta']={key:after[key]-before[key] for key in ('user_s','system_s')}
            if not c['allow_materialized']:
                assert 'total' in (d/'syscalls.txt').read_text(),'no syscall evidence; inspect, do not silently pass'
        save(d/'result.json',row);print(json.dumps(row),flush=True)
        assert not list(Path(c['temporary']).iterdir()),'owned invocation storage remains'
        return row
    except BaseException:
        for name in ('abort','go','finish'):(d/name).touch()
        if target is not None and tracer(target)==launcher_pid:
            try:os.kill(launcher_pid,signal.SIGINT)
            except ProcessLookupError:pass
        if server is not None:
            try:os.killpg(server,signal.SIGTERM)
            except ProcessLookupError:pass
        end=time.monotonic()+10
        while time.monotonic()<end:
            collect()
            if server in reaped:break
            time.sleep(.01)
        save(d/'failed-processes.json',reaped)
        raise

def baseline(c,script):
    d=Path(c['case']);d.mkdir();clone=Path(c['temporary']).parent/'clone'
    before=resource.getrusage(resource.RUSAGE_CHILDREN)
    own=resource.getrusage(resource.RUSAGE_SELF);begin=time.monotonic_ns()
    env={k:v for k,v in os.environ.items() if not k.startswith(('GIT_','SSH_','LD_PRELOAD'))}
    env.update(LC_ALL='C',GIT_CONFIG_NOSYSTEM='1',GIT_CONFIG_GLOBAL=os.devnull,TMPDIR=c['temporary'])
    subprocess.run(['git','-c','core.hooksPath=/dev/null','clone','--shared','--quiet',c['repo'],str(clone)],env=env,check=True,timeout=30)
    materialize_ns=time.monotonic_ns()-begin
    child=dict(c,trace='',allow_materialized=True);save(d/'config.json',child)
    started=time.monotonic_ns()
    result=subprocess.run([sys.executable,'-I','-S',str(script),'--payload',str(d/'config.json')],env=env,cwd=clone,capture_output=True,timeout=30)
    command_ns=time.monotonic_ns()-started
    assert result.returncode==0,result.stderr
    started=time.monotonic_ns();shutil.rmtree(clone);cleanup_ns=time.monotonic_ns()-started
    after=resource.getrusage(resource.RUSAGE_CHILDREN);now=resource.getrusage(resource.RUSAGE_SELF)
    row=dict(case=d.name,elapsed_ns=time.monotonic_ns()-begin,materialize_ns=materialize_ns,
             direct_command_ns=command_ns,cleanup_ns=cleanup_ns,
             waited_user_s=after.ru_utime-before.ru_utime,waited_system_s=after.ru_stime-before.ru_stime,
             supervisor_user_s=now.ru_utime-own.ru_utime,supervisor_system_s=now.ru_stime-own.ru_stime,
             child=json.loads((d/'done.json').read_text()))
    save(d/'result.json',row);print(json.dumps(row),flush=True)

def main():
    if len(sys.argv)==3 and sys.argv[1]=='--payload':
        raise SystemExit(payload(json.loads(Path(sys.argv[2]).read_text())))
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--binary',type=Path,required=True);p.add_argument('--checkout',type=Path,required=True)
    p.add_argument('--work',type=Path,required=True);p.add_argument('--output',type=Path,required=True)
    p.add_argument('--files',type=int,default=8192);p.add_argument('--bytes',type=int,default=4096)
    p.add_argument('--allow-materialized',action='store_true',help='local supervisor self-test only; NOT a brick FUSE result')
    a=p.parse_args();assert 0<a.files<=8192 and 8<=a.bytes<=8192 and a.files*a.bytes<=32*1024**2
    assert a.files*a.bytes>16*1024**2,'must exceed the payload LRU'
    assert sys.platform=='linux' and shutil.which('strace')
    assert a.allow_materialized or Path('/dev/fuse').is_char_device()
    head=subprocess.check_output(['git','-C',str(a.checkout),'rev-parse','HEAD'],text=True).strip();assert head==HEAD,head
    assert not subprocess.check_output(['git','-C',str(a.checkout),'status','--porcelain'])
    for d in (a.work,a.output):d.mkdir(parents=True);assert not list(d.iterdir())
    # Process-local supervision only. No host settings or tracing exemptions.
    assert ctypes.CDLL(None,use_errno=True).prctl(36,1,0,0,0)==0,'PR_SET_CHILD_SUBREAPER failed'
    repo=a.work/'source.git';temporary=a.work/'temporary';temporary.mkdir()
    env={k:v for k,v in os.environ.items() if not k.startswith(('GIT_','SSH_','LD_PRELOAD'))}
    env.update(LC_ALL='C',GIT_CONFIG_NOSYSTEM='1',GIT_CONFIG_GLOBAL=os.devnull)
    def git(*args,**kw):return subprocess.run(['git',*map(str,args)],env=env,check=True,capture_output=True,**kw)
    git('init','--bare','--quiet',repo)
    with (a.work/'import').open('wb') as f:
        for i in range(a.files):
            data=i.to_bytes(8,'little')+hashlib.shake_256(f'brick-fuse:{i}'.encode()).digest(a.bytes-8)
            f.write(f'blob\nmark :{i+1}\ndata {a.bytes}\n'.encode()+data+b'\n')
        f.write(b'commit refs/heads/main\ncommitter Fixture <fixture@example.invalid> 0 +0000\ndata 7\nfixture\n')
        for i in range(a.files):f.write(f'M 100644 :{i+1} f{i:05d}\n'.encode())
        f.write(b'\ndone\n')
    with (a.work/'import').open('rb') as f:git('-C',repo,'fast-import','--quiet',stdin=f)
    (a.work/'import').unlink();git('-C',repo,'symbolic-ref','HEAD','refs/heads/main')
    git('-C',repo,'-c','pack.window=0','-c','pack.depth=0','repack','-ad')
    refs=git('-C',repo,'show-ref').stdout
    save(a.output/'environment.json',dict(head=head,binary_sha256=hashlib.sha256(a.binary.read_bytes()).hexdigest(),
        files=a.files,bytes=a.bytes,kernel=os.uname().release,affinity=sorted(os.sched_getaffinity(0)),
        filesystem=subprocess.check_output(['stat','-f','-c','%T',str(a.work)],text=True).strip(),
        cache='warm/uncontrolled, no cache drops/advice',note='Traced barriers and ptrace overhead are diagnostic, not benchmarks. Untraced runs include mount/command/cleanup. No full WebKit acceptance.'))
    script=Path(__file__).resolve()
    for name,trace in (('baseline-1','baseline'),('plain-1',''),('server','server'),('requester','requester'),('command','command'),('plain-2',''),('baseline-2','baseline')):
        c=dict(case=str((a.output/name).resolve()),binary=str(a.binary.resolve()),
            repo=str(repo.resolve()),temporary=str(temporary.resolve()),files=a.files,size=a.bytes,
            trace=trace,allow_materialized=a.allow_materialized)
        if trace=='baseline':baseline(c,script)
        else:run_case(c,script)
        assert git('-C',repo,'show-ref').stdout==refs,'source pin/refs leaked'
        assert not any(str(temporary.resolve()) in line for line in Path('/proc/self/mountinfo').read_text().splitlines()),'mount remains'
    assert not list(temporary.iterdir())
    shutil.rmtree(a.work)
    print('SUCCESS: real-FUSE requirement checked in every payload; child/server exit, detach, pins and owned storage checked.' if not a.allow_materialized else 'SELF-TEST ONLY: supervisor/late-attach/cleanup checked without FUSE.',flush=True)

if __name__=='__main__':main()
