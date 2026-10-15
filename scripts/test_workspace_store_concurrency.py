#!/usr/bin/env python3
"""Hermetic two-MCP-process integration for shared trusted Workspace CAS/live reads.

This is a cross-process test; it does NOT prove multiple MCP clients in one process.
"""
import concurrent.futures
import json
import os
from pathlib import Path
import select
import subprocess
import tempfile
import time

binary = os.environ['LEANCTX_TEST_BIN']
base = Path(tempfile.mkdtemp(prefix='leanctx-workspace-cas-'))
for name in ('a', 'b', 'c', 'd', 'outside'):
    repo = base / ('repo-' + name)
    repo.mkdir()
    (repo / '.git').mkdir()
    (repo / ('source-' + name + '.txt')).write_text('cobalt lantern visible ' + name)
for name in ('home-a','home-b','home-c','home-d','data-a','data-b','data-c','data-d','tmp'):
    (base / name).mkdir()
file = base / 'workspace.json'
file.write_text(json.dumps({'id': '11111111-1111-4111-8111-111111111111',
                           'revision': 1, 'primary': '/fixture/repo-a', 'additional': []}))
file.chmod(0o600)
second = base / 'workspace-other.json'
second.write_text(json.dumps({'id': '22222222-2222-4222-8222-222222222222',
                             'revision': 1, 'primary': '/fixture/repo-c', 'additional': []}))
second.chmod(0o600)
third = base / 'workspace-same-primary.json'
third.write_text(json.dumps({'id': '33333333-3333-4333-8333-333333333333',
                             'revision': 1, 'primary': '/fixture/repo-a', 'additional': []}))
third.chmod(0o600)

class Client:
    def __init__(self, tag, root, store):
        command = ['bwrap','--die-with-parent','--unshare-net',
                   '--ro-bind','/usr','/usr','--ro-bind','/lib','/lib',
                   '--ro-bind','/lib64','/lib64','--proc','/proc','--dev','/dev',
                   '--dir','/bin','--ro-bind',binary,'/bin/leanctx',
                   '--bind',str(base),'/fixture','--tmpfs','/tmp',
                   '--chdir','/fixture/repo-'+root,
                   '--setenv','HOME','/fixture/home-'+tag,
                   '--setenv','LEAN_CTX_DATA_DIR','/fixture/data-'+tag,
                   '--setenv','LEAN_CTX_TEST_REVOCATION_MARKER','/fixture/revocation-'+tag,
                   '--setenv','LEAN_CTX_STRICT_WORKSPACE','1',
                   '--setenv','LEAN_CTX_WORKSPACE_FILE','/fixture/'+store,

                   '--','/bin/leanctx','mcp']
        self.p = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=subprocess.DEVNULL, text=True, bufsize=1)
        self.serial = 0
        self.rpc('initialize', {'protocolVersion':'2025-06-18','capabilities':{},
                                'clientInfo':{'name':'concurrency-'+tag,'version':'1'}})
        self.p.stdin.write(json.dumps({'jsonrpc':'2.0','method':'notifications/initialized','params':{}})+'\n')
        self.p.stdin.flush()
    def rpc(self, method, params):
        self.serial += 1
        id = self.serial
        self.p.stdin.write(json.dumps({'jsonrpc':'2.0','id':id,'method':method,'params':params})+'\n')
        self.p.stdin.flush()
        deadline = time.monotonic()+35
        while time.monotonic() < deadline:
            ready,_,_ = select.select([self.p.stdout], [], [], max(0,deadline-time.monotonic()))
            if not ready: break
            line = self.p.stdout.readline()
            if not line: break
            obj = json.loads(line)
            if obj.get('id') == id: return obj
        raise RuntimeError('MCP timeout/closed: '+method)
    def tool(self, name, arguments=None):
        reply = self.rpc('tools/call', {'name':name,'arguments':arguments or {}})
        if 'error' in reply: return reply
        txt = next((block['text'] for block in reply.get('result',{}).get('content',[])
                    if block.get('type')=='text'), '')
        if name.startswith('ctx_workspace_'):
            return json.loads(txt)
        return txt
    def close(self):
        self.p.terminate()
        try:self.p.wait(timeout=3)
        except subprocess.TimeoutExpired:self.p.kill();self.p.wait()

clients=[]
try:
    a = Client('a','a','workspace.json'); clients.append(a)
    b = Client('b','a','workspace.json'); clients.append(b)
    c = Client('c','c','workspace-other.json'); clients.append(c)
    d = Client('d','a','workspace-same-primary.json'); clients.append(d)
    first = a.tool('ctx_workspace_get')
    other = b.tool('ctx_workspace_get')
    isolated = c.tool('ctx_workspace_get')
    assert first['workspaceId']==other['workspaceId'] and first['revision']==1
    assert isolated['workspaceId']!=first['workspaceId']
    assert d.tool('ctx_workspace_get')['workspaceId']!=first['workspaceId']
    denied_other = c.tool('ctx_search',{'query':'cobalt lantern','path':'/fixture/repo-b'})
    assert 'outside' in denied_other or 'escapes project root' in denied_other, denied_other
    def admin(action, store, root, revision):
        command=['bwrap','--die-with-parent','--unshare-net',
                 '--ro-bind','/usr','/usr','--ro-bind','/lib','/lib','--ro-bind','/lib64','/lib64',
                 '--proc','/proc','--dev','/dev','--dir','/bin','--ro-bind',binary,'/bin/leanctx',
                 '--bind',str(base),'/fixture','--tmpfs','/tmp','--setenv','HOME','/fixture/home-a',
                 '--','/bin/leanctx','workspace-admin',action,'/fixture/'+store,root,str(revision)]
        result=subprocess.run(command,capture_output=True,text=True,timeout=40)
        return json.loads(result.stdout) if result.returncode==0 else {'error':result.stderr}

    added = admin('add','workspace.json','/fixture/repo-b',1)
    assert added['revision']==2
    observed = b.tool('ctx_workspace_get')
    assert observed['revision']==2 and '/fixture/repo-b' in observed['additionalRoots']
    assert 'visible b' in b.tool('ctx_search',{'query':'cobalt lantern','path':'/fixture/repo-b'})
    # Same primary root, different additional roots, same query and warm cache.
    for _ in range(3):
        denied_d=d.tool('ctx_search',{'query':'cobalt lantern','path':'/fixture/repo-b'})
        assert 'outside' in denied_d or 'escapes project root' in denied_d,denied_d
    print('PASS: warm-cache same-primary separate-authority rejection')
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
        f1=pool.submit(admin,'add','workspace.json','/fixture/repo-c',2)
        f2=pool.submit(admin,'add','workspace.json','/fixture/repo-d',2)
        outcomes=[f1.result(),f2.result()]
    assert sum(isinstance(outcome,dict) and outcome.get('revision')==3 for outcome in outcomes)==1,outcomes
    assert sum(isinstance(outcome,dict) and 'error' in outcome for outcome in outcomes)==1,outcomes
    marker=base/'revocation-b'
    marker.unlink(missing_ok=True)
    with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
        flight=pool.submit(b.tool,'ctx_search',{'query':'cobalt lantern','path':'/fixture/repo-b'})
        deadline=time.monotonic()+25
        while not marker.exists() and time.monotonic()<deadline:
            time.sleep(0.01)
        assert marker.exists(),'Search never reached publication fence'
        removed=admin('remove','workspace.json','/fixture/repo-b',3)
        raced=flight.result(timeout=35)
    assert isinstance(raced,dict) and 'error' in raced,raced
    print('PASS: in-flight response suppressed after root revocation')
    assert removed['revision']==4 and '/fixture/repo-b' not in removed['additional']
    denied=b.tool('ctx_search',{'query':'cobalt lantern','path':'/fixture/repo-b'})
    assert 'outside' in denied or 'escapes project root' in denied,denied
    assert b.tool('ctx_workspace_get')['revision']==4
    assert c.tool('ctx_workspace_get')['revision']==1
    assert c.tool('ctx_workspace_get')['workspaceId']==isolated['workspaceId']
    # A new process must recover the same workspace ID and committed revision.
    a.close(); clients.remove(a)
    restarted = Client('a','a','workspace.json'); clients.append(restarted)
    reloaded = restarted.tool('ctx_workspace_get')
    assert reloaded['revision']==4 and reloaded['workspaceId']==first['workspaceId'],reloaded
    assert c.tool('ctx_workspace_get')['workspaceId']==isolated['workspaceId']
    # ABA: removing and re-adding a root does not restore an old revision.
    readded=admin('add','workspace.json','/fixture/repo-b',4)
    assert readded['revision']==5,readded
    assert 'visible b' in restarted.tool('ctx_search',{'query':'cobalt lantern','path':'/fixture/repo-b'})
    assert 'error' in admin('remove','workspace.json','/fixture/repo-b',3)
    assert 'error' in admin('remove','workspace.json','/fixture/repo-b',4)
    removed_again=admin('remove','workspace.json','/fixture/repo-b',5)
    assert removed_again['revision']==6,removed_again
    assert 'escapes project root' in restarted.tool('ctx_search',{'query':'cobalt lantern','path':'/fixture/repo-b'})
    # A world-readable or symlinked state file cannot be accepted by admin.
    file.chmod(0o644)
    assert 'error' in admin('add','workspace.json','/fixture/repo-b',6)
    file.chmod(0o600)
    evil=base/'evil.json'
    evil.symlink_to(file)
    assert 'error' in admin('add','evil.json','/fixture/repo-b',6)
    assert restarted.tool('ctx_workspace_get')['revision']==6
    print('PASS: ABA denial, 0600 and symlink protections')
    print('PASS: shared-store CAS, live add/remove, isolated other workspace, revision stability')
finally:
    for c in clients:c.close()
