#!/usr/bin/env python3
"""Real stdio MCP regression for Harr's backward-compatible Workspace tool surface."""
import json, os, select, subprocess, tempfile, time
from pathlib import Path

binary = Path(os.environ['LEANCTX_TEST_BIN']).resolve()
with tempfile.TemporaryDirectory(prefix='leanctx-harr-legacy-') as td:
    base=Path(td); home=base/'home'; primary=home/'Projects'/'repo-a'; extra=home/'Projects'/'repo-b'; wrong=home/'outer'
    for p in (primary,extra,wrong): p.mkdir(parents=True,exist_ok=True)
    for p in (primary,extra): (p/'.git').mkdir()
    (home/'.git').mkdir(); (home/'package.json').write_text('{}\n')
    cfg=base/'config'/'lean-ctx'; cfg.mkdir(parents=True)
    (cfg/'config.toml').write_text('''shadow_mode = true
permission_inheritance = "off"
prefer_native_editor = true
rules_injection = "off"
minimal_overhead = true
tool_profile = ""
tools_enabled = ["ctx_read", "ctx_shell", "ctx_search", "ctx_glob", "ctx_tools", "ctx_call"]
disabled_tools = ["ctx_edit", "ctx_patch"]
default_tool_categories = ["core"]
[gateway]
enabled = true
''')
    (primary/'marker-a.txt').write_text('primary-marker\n'); (extra/'marker-b.txt').write_text('extra-marker\n')
    store=base/'workspace.json'
    store.write_text(json.dumps({'id':'11111111-1111-4111-8111-111111111111','revision':1,'primary':str(primary.resolve()),'additional':[str(extra.resolve())]})); store.chmod(0o600)
    env=os.environ.copy(); env.update({
        'HOME':str(home), 'LEAN_CTX_DATA_DIR':str(base/'data'), 'XDG_CONFIG_HOME':str(base/'config'),
        'LEAN_CTX_CONFIG_DIR':str(cfg),
        'LEAN_CTX_STRICT_WORKSPACE':'1', 'LEAN_CTX_STRICT_WORKSPACE_LEGACY_TOOLS':'1',
        'LEAN_CTX_WORKSPACE_FILE':str(store), 'LEAN_CTX_HEADLESS':'1'})
    p=subprocess.Popen([str(binary),'mcp'],cwd=primary,env=env,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,bufsize=1)
    seq=0
    def rpc(method,params,timeout=30):
        global seq
        seq+=1; ident=seq
        p.stdin.write(json.dumps({'jsonrpc':'2.0','id':ident,'method':method,'params':params})+'\n'); p.stdin.flush()
        end=time.monotonic()+timeout
        while time.monotonic()<end:
            ready,_,_=select.select([p.stdout],[],[],1)
            if ready:
                line=p.stdout.readline()
                if line:
                    obj=json.loads(line)
                    if obj.get('id')==ident:return obj
            if p.poll() is not None: break
        raise RuntimeError(('timeout',method,p.stderr.read()))
    def call(name,args): return rpc('tools/call',{'name':name,'arguments':args})
    def text(reply):
        if 'error' in reply:return json.dumps(reply['error'])
        return '\n'.join(x.get('text','') for x in reply.get('result',{}).get('content',[]) if x.get('type')=='text')
    try:
        assert 'result' in rpc('initialize',{'protocolVersion':'2025-06-18','capabilities':{},'clientInfo':{'name':'harr-legacy-surface-test','version':'1'}})
        p.stdin.write(json.dumps({'jsonrpc':'2.0','method':'notifications/initialized','params':{}})+'\n'); p.stdin.flush()
        names={x['name'] for x in rpc('tools/list',{})['result']['tools']}
        legacy={'ctx_call','ctx_glob','ctx_read','ctx_search','ctx_shell','ctx_tools'}
        assert legacy <= names,(legacy-names,names)
        assert {'ctx_workspace_get','ctx_workspace_list','ctx_workspace_diagnose'} <= names,names

        default_shell=text(call('ctx_shell',{'command':'pwd'}))
        assert str(primary.resolve()) in default_shell,(default_shell,primary)

        extra_shell=text(call('ctx_shell',{'command':'pwd','cwd':str(extra.resolve())}))
        assert str(extra.resolve()) in extra_shell,extra_shell

        glob=text(call('ctx_glob',{'pattern':'marker-b.txt','path':str(extra.resolve())}))
        assert 'marker-b.txt' in glob,glob

        catalog=call('ctx_tools',{'action':'list'})
        assert 'error' not in catalog,catalog

        outside=base/'outside'; outside.mkdir(); (outside/'x').write_text('x')
        denied=text(call('ctx_glob',{'pattern':'*','path':str(outside.resolve())}))
        assert 'outside' in denied or 'escapes project root' in denied or 'request authority' in denied,denied
        print('PASS: Harr strict Workspace preserves pre-Workspace tool surface and binds shell cwd to primary/additional roots')
    finally:
        p.terminate()
        try:p.wait(timeout=3)
        except subprocess.TimeoutExpired:p.kill();p.wait(timeout=3)
