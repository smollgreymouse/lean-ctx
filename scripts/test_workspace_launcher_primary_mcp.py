#!/usr/bin/env python3
import json, os, select, subprocess, tempfile, time, uuid
from pathlib import Path

binary=Path(os.environ['LEANCTX_TEST_BIN']).resolve()
with tempfile.TemporaryDirectory(prefix='leanctx-launcher-primary-') as td:
    base=Path(td); home=base/'home'; repo=home/'Projects'/'repo-a'; state=base/'state'; data=base/'data'; cfg=base/'config'
    for p in (repo,state,data,cfg): p.mkdir(parents=True,exist_ok=True)
    (repo/'.git').mkdir(); (repo/'README.md').write_text('launcher primary marker\n')
    # Reproduce a real workstation where HOME itself also looks like a project.
    (home/'.git').mkdir(); (home/'package.json').write_text('{}\n')
    store=state/'workspace.json'
    store.write_text(json.dumps({'id':str(uuid.uuid4()),'revision':1,'primary':str(repo.resolve()),'additional':[]}))
    store.chmod(0o600)
    env=os.environ.copy(); env.update({
        'HOME':str(home),'XDG_CONFIG_HOME':str(cfg),'LEAN_CTX_DATA_DIR':str(data),
        'LEAN_CTX_STRICT_WORKSPACE':'1','LEAN_CTX_WORKSPACE_FILE':str(store),'LEAN_CTX_HEADLESS':'1'})
    p=subprocess.Popen([str(binary),'mcp'],cwd=repo,env=env,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,bufsize=1)
    seq=0
    def rpc(method,params,timeout=30):
        global seq
        seq+=1; ident=seq
        p.stdin.write(json.dumps({'jsonrpc':'2.0','id':ident,'method':method,'params':params})+'\n');p.stdin.flush()
        end=time.monotonic()+timeout
        while time.monotonic()<end:
            ready,_,_=select.select([p.stdout],[],[],1)
            if ready:
                line=p.stdout.readline()
                if line:
                    obj=json.loads(line)
                    if obj.get('id')==ident:return obj
            if p.poll() is not None: break
        raise RuntimeError('MCP timeout')
    def tool(name,args=None): return rpc('tools/call',{'name':name,'arguments':args or {}})
    try:
        init=rpc('initialize',{'protocolVersion':'2025-06-18','capabilities':{},'clientInfo':{'name':'launcher-primary-test','version':'1'}})
        assert 'result' in init,init
        p.stdin.write(json.dumps({'jsonrpc':'2.0','method':'notifications/initialized','params':{}})+'\n');p.stdin.flush()
        ws=tool('ctx_workspace_get'); assert 'error' not in ws,ws
        txt='\n'.join(x.get('text','') for x in ws['result']['content'] if x.get('type')=='text')
        doc=json.loads(txt); assert doc['primaryRoot']==str(repo.resolve()),doc
        rd=tool('ctx_read',{'path':str(repo/'README.md')}); assert 'error' not in rd,rd
        content='\n'.join(x.get('text','') for x in rd['result']['content'] if x.get('type')=='text')
        assert 'launcher primary marker' in content,content
        print('PASS: launcher-bound Workspace primary overrides unsafe outer HOME project marker')
    finally:
        p.terminate()
        try:p.wait(timeout=3)
        except subprocess.TimeoutExpired:p.kill();p.wait(timeout=5)
