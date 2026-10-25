#!/usr/bin/env python3
import json, os, pathlib, shutil, subprocess, tempfile, time, select
binary=os.environ['LEANCTX_TEST_BIN']
docker=shutil.which('docker')
if not docker:
    print('NOT EXECUTED: docker unavailable for disposable multi-UID fixture'); raise SystemExit(77)
img=subprocess.run([docker,'image','ls','-q'],text=True,capture_output=True,timeout=10).stdout.splitlines()
if not img:
    print('NOT EXECUTED: no local docker image; test will not pull from network'); raise SystemExit(77)
image=img[0]
base=pathlib.Path(tempfile.mkdtemp(prefix='.tmp-leanctx-multiuid-', dir=os.getcwd()))
base.chmod(0o777)
try:
    for name in ('root','extra','state','home-a','home-b','data-a','data-b'):
        p=base/name; p.mkdir(); p.chmod(0o777)
    (base/'root'/'.git').mkdir(); (base/'extra'/'.git').mkdir()
    os.chmod(base/'root'/'.git',0o777); os.chmod(base/'extra'/'.git',0o777)
    def cmd(uid,*args,env=None,check=False):
        c=[docker,'run','--rm','--network','none','--user',f'{uid}:{uid}',
           '-v','/:/host:ro','-v',f'{binary}:/leanctx:ro','-v',f'{base}:/fixture:rw']
        for k,v in (env or {}).items(): c += ['-e',f'{k}={v}']
        c += ['--entrypoint','/host/lib64/ld-linux-x86-64.so.2',image,
              '--library-path','/host/lib/x86_64-linux-gnu:/host/usr/lib/x86_64-linux-gnu',
              '/leanctx',*map(str,args)]
        r=subprocess.run(c,text=True,capture_output=True,timeout=40)
        if check and r.returncode: raise AssertionError((c,r.returncode,r.stdout,r.stderr))
        return r
    create=cmd(2001,'workspace-admin','create','/fixture/state/workspace.json','/fixture/root',check=True)
    owner=cmd(2001,'workspace-admin','show','/fixture/state/workspace.json',check=True)
    state=json.loads(owner.stdout); assert state['primary']=='/fixture/root'
    assert cmd(2002,'workspace-admin','show','/fixture/state/workspace.json').returncode != 0
    assert cmd(2002,'workspace-admin','add','/fixture/state/workspace.json','/fixture/extra','1').returncode != 0
    st=os.stat(base/'state'/'workspace.json'); lk=os.stat(base/'state'/'workspace.json.lock')
    assert st.st_uid==2001 and lk.st_uid==2001,(st.st_uid,lk.st_uid)
    def mcp(uid, tag):
        env={'HOME':f'/fixture/home-{tag}','LEAN_CTX_DATA_DIR':f'/fixture/data-{tag}',
             'LEAN_CTX_STRICT_WORKSPACE':'1','LEAN_CTX_WORKSPACE_FILE':'/fixture/state/workspace.json'}
        c=[docker,'run','--rm','--network','none','--user',f'{uid}:{uid}','-i',
           '-v','/:/host:ro','-v',f'{binary}:/leanctx:ro','-v',f'{base}:/fixture:rw',
           '-w','/fixture/root']
        for k,v in env.items(): c += ['-e',f'{k}={v}']
        c += ['--entrypoint','/host/lib64/ld-linux-x86-64.so.2',image,
              '--library-path','/host/lib/x86_64-linux-gnu:/host/usr/lib/x86_64-linux-gnu','/leanctx','mcp']
        p=subprocess.Popen(c,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,bufsize=1)
        def rpc(i,method,params):
            p.stdin.write(json.dumps({'jsonrpc':'2.0','id':i,'method':method,'params':params})+'\n');p.stdin.flush()
            end=time.monotonic()+30
            while time.monotonic()<end:
                ready,_,_=select.select([p.stdout],[],[],1)
                if ready:
                    obj=json.loads(p.stdout.readline())
                    if obj.get('id')==i:return obj
            raise AssertionError('MCP timeout')
        rpc(1,'initialize',{'protocolVersion':'2025-06-18','capabilities':{},'clientInfo':{'name':'uid-test','version':'1'}})
        p.stdin.write(json.dumps({'jsonrpc':'2.0','method':'notifications/initialized','params':{}})+'\n');p.stdin.flush()
        out=rpc(2,'tools/call',{'name':'ctx_workspace_get','arguments':{}})
        p.terminate()
        try: p.wait(timeout=3)
        except subprocess.TimeoutExpired:
            p.kill(); p.wait(timeout=5)
        return out
    good=mcp(2001,'a'); assert 'error' not in good,good
    bad=mcp(2002,'b'); assert 'error' in bad,bad
    print('PASS: UID owner can use Workspace; foreign UID denied in admin CLI and MCP')
finally:
    # Bind-mounted files are owned by synthetic container UIDs. Restore the
    # fixture ownership through a disposable root container so local cleanup is
    # complete and the repository never retains untracked UID-test artifacts.
    try:
        subprocess.run([
            docker,'run','--rm','--network','none','--user','0:0',
            '-v','/:/host:ro','-v',f'{base}:/fixture:rw',
            '--entrypoint','/host/lib64/ld-linux-x86-64.so.2',image,
            '--library-path','/host/lib/x86_64-linux-gnu:/host/usr/lib/x86_64-linux-gnu',
            '/host/usr/bin/chown','-R',f'{os.getuid()}:{os.getgid()}','/fixture'
        ],capture_output=True,text=True,timeout=20)
    except Exception:
        pass
    shutil.rmtree(base,ignore_errors=True)
