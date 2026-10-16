#!/usr/bin/env python3
"""Standalone real-MCP PathJail/search authority regression, Linux bubblewrap only.

Run with LEANCTX_TEST_BIN=/absolute/path/to/real/lean-ctx and one of
`default`, `allow`, `extra`, `strict-default`, `strict-allow`, `strict-extra`, `strict-roots`. No production config or projects are mounted.
"""
import json, os, pathlib, subprocess, tempfile, select, time, sys
b=pathlib.Path(tempfile.mkdtemp(prefix='harr-leanctx-baseline-'))
for n in ['a','b','outside']:
 d=b/('repo-'+n); d.mkdir(); (d/'.git').mkdir(); (d/('unique-'+n+'.txt')).write_text('cobalt lantern walnut '+n)
(b/'repo-a'/'escape').symlink_to('/fixture/repo-outside', target_is_directory=True)
for n in ['home','data','xdg-config','xdg-cache','xdg-data','tmp']: (b/n).mkdir()
real=os.environ.get('LEANCTX_TEST_BIN')
if not real or not pathlib.Path(real).is_file():
 raise SystemExit('Set LEANCTX_TEST_BIN to the absolute path of an existing LeanCTX binary; NEVER point to a wrapper')
cmd=['bwrap','--die-with-parent','--unshare-net','--ro-bind','/usr','/usr','--ro-bind','/lib','/lib','--ro-bind','/lib64','/lib64','--proc','/proc','--dev','/dev','--dir','/bin','--ro-bind',real,'/bin/leanctx','--bind',str(b),'/fixture','--tmpfs','/tmp','--chdir','/fixture/repo-a','--setenv','HOME','/fixture/home','--setenv','LEAN_CTX_DATA_DIR','/fixture/data','--setenv','XDG_CONFIG_HOME','/fixture/xdg-config','--setenv','XDG_CACHE_HOME','/fixture/xdg-cache','--setenv','XDG_DATA_HOME','/fixture/xdg-data','--setenv','TMPDIR','/fixture/tmp']
mode=sys.argv[1] if len(sys.argv)>1 else 'default'
strict = mode.startswith('strict-')
if strict:
 mode = mode[len('strict-'):]
assert mode in ('default','allow','extra','roots','store','store-forged','create')
if mode in ('store','store-forged','create'):
 store = b/'workspace.json'
 if mode in ('store','store-forged'):
  store.write_text(json.dumps({'id':'11111111-1111-4111-8111-111111111111','revision':1,'primary':'/fixture/repo-a','additional':[]}))
  store.chmod(0o600)
 cmd += ['--setenv','LEAN_CTX_WORKSPACE_FILE','/fixture/workspace.json',
]
if mode=='extra':cmd+=['--setenv','LEAN_CTX_EXTRA_ROOTS','/fixture/repo-b']
if mode=='allow':cmd+=['--setenv','LEAN_CTX_ALLOW_PATH','/fixture/repo-b']
if strict:
 cmd += ['--setenv','LEAN_CTX_STRICT_WORKSPACE','1']
cmd+=['--','/bin/leanctx','mcp']
print('MODE',mode,'FIXTURE',b,flush=True)
p=subprocess.Popen(cmd,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,bufsize=1)
def request(obj,limit=18):
 p.stdin.write(json.dumps(obj)+'\n');p.stdin.flush();deadline=time.monotonic()+limit
 while time.monotonic()<deadline:
  ready,_,_=select.select([p.stdout],[],[],max(0,deadline-time.monotonic()))
  if not ready: break
  line=p.stdout.readline()
  if not line:break
  try:
   msg=json.loads(line)
   if msg.get('method')=='roots/list' and 'id' in msg:
    roots = [{'uri':'file:///fixture/repo-a','name':'first'}, {'uri':'file:///fixture/repo-b','name':'second'}]
    p.stdin.write(json.dumps({'jsonrpc':'2.0','id':msg['id'],'result':{'roots':roots}})+'\n');p.stdin.flush()
    continue
   if msg.get('id')==obj['id']: return msg
  except Exception: pass
 return {'error':'timeout or closed','returncode':p.poll()}
try:
 init=request({'jsonrpc':'2.0','id':1,'method':'initialize','params':{'protocolVersion':'2025-06-18','capabilities':({'roots':{'listChanged':True}} if mode in ('roots','store-forged') else {}),'clientInfo':{'name':'isolated-baseline','version':'1'}}},30)
 print('INIT',str(init)[:1000],flush=True)
 if 'result' in init:
  p.stdin.write(json.dumps({'jsonrpc':'2.0','method':'notifications/initialized','params':{}})+'\n');p.stdin.flush()
  tools=request({'jsonrpc':'2.0','id':2,'method':'tools/list','params':{}},18)
  names=[t.get('name') for t in tools.get('result',{}).get('tools',[])]
  assert ('ctx_workspace_get' in names)==strict, ('workspace tool discovery',strict,names)
  assert ('ctx_workspace_list' in names)==strict
  assert 'ctx_workspace_add_root' not in names and 'ctx_workspace_create' not in names and 'ctx_workspace_remove_root' not in names
  assert ('ctx_workspace_diagnose' in names)==strict
  if strict:
   assert 'ctx_shell' not in names and 'ctx_call' not in names, names
   def admin(action, *arguments):
    command=cmd[:-3] + ['--','/bin/leanctx','workspace-admin',action,'/fixture/workspace.json',*map(str,arguments)]
    return subprocess.run(command,capture_output=True,text=True,timeout=25)
   if mode=='create':
    initial=admin('create','/fixture/repo-a')
    assert initial.returncode==0,(initial.stdout,initial.stderr)
    assert store.exists() and store.stat().st_mode & 0o077 == 0
    assert admin('create','/fixture/repo-a').returncode!=0
   reject=request({'jsonrpc':'2.0','id':16,'method':'tools/call','params':{'name':'ctx_shell','arguments':{'command':'pwd'}}},25)
   assert 'error' in reject,reject
   resource=request({'jsonrpc':'2.0','id':15,'method':'resources/read',
                     'params':{'uri':'file:///fixture/repo-outside/unique-outside.txt'}},25)
   assert 'error' in resource,resource
   result=request({'jsonrpc':'2.0','id':20,'method':'tools/call','params':{'name':'ctx_workspace_get','arguments':{}}},25)
   parts=result.get('result',{}).get('content',[])
   document=json.loads(next(x['text'] for x in parts if x.get('type')=='text'))
   assert document['primaryRoot']=='/fixture/repo-a',document
   assert document['additionalRoots']==(['/fixture/repo-b'] if mode=='roots' else []), document
   assert document['authenticatedWorkspace'] is (mode in ('store','store-forged','create')),document
   assert document['mutable'] is (mode in ('store','store-forged','create')),document
   assert document['revision']==1,document
   assert isinstance(document['workspaceId'],str) and len(document['workspaceId'])>10
   for index, tool in enumerate(('ctx_workspace_get','ctx_workspace_list','ctx_workspace_diagnose'),21):
    answer=request({'jsonrpc':'2.0','id':index,'method':'tools/call','params':{'name':tool,'arguments':{}}},25)
    obj=json.loads(next(x['text'] for x in answer['result']['content'] if x.get('type')=='text'))
    candidate= obj if tool=='ctx_workspace_get' else (obj['workspaces'][0] if tool=='ctx_workspace_list' else obj['workspace'])
    assert candidate['workspaceId']==document['workspaceId']
    assert candidate['revision']==document['revision']
    if tool=='ctx_workspace_list': assert len(obj['workspaces'])==1
    if tool=='ctx_workspace_diagnose': assert obj['status'] in ('read_only_untrusted_roots','launcher_bound')

   print('WORKSPACE_API',json.dumps(document),flush=True)
   if mode=='store-forged':
    forged=request({'jsonrpc':'2.0','id':47,'method':'tools/call','params':{
      'name':'ctx_search','arguments':{'query':'cobalt lantern','path':'/fixture/repo-b'},
      '_meta':{'workspaceId':'22222222-2222-4222-8222-222222222222','threadId':'forged-thread'}}},25)
    forged_text=' '.join(x.get('text','') for x in forged.get('result',{}).get('content',[]))
    assert 'outside' in forged_text or 'escapes project root' in forged_text,forged_text
    print('FORGED_CLIENT_AUTHORITY_DENIED PASS',flush=True)
   if mode in ('store','store-forged','create'):
    added=admin('add','/fixture/repo-b',1)
    assert added.returncode==0,(added.stdout,added.stderr)
    assert json.loads(added.stdout)['revision']==2
    assert admin('add','/fixture/repo-b',1).returncode!=0
    search_reply=request({'jsonrpc':'2.0','id':43,'method':'tools/call','params':{'name':'ctx_search','arguments':{'query':'cobalt lantern','path':'/fixture/repo-b'}}},25)
    search_text=' '.join(x.get('text','') for x in search_reply.get('result',{}).get('content',[]))
    assert '1 results' in search_text and 'cobalt lantern walnut b' in search_text,search_text
    removed=admin('remove','/fixture/repo-b',2)
    assert removed.returncode==0,(removed.stdout,removed.stderr)
    assert json.loads(removed.stdout)['revision']==3
    response=request({'jsonrpc':'2.0','id':45,'method':'tools/call','params':{'name':'ctx_search','arguments':{'query':'cobalt lantern','path':'/fixture/repo-b'}}},25)
    text=' '.join(x.get('text','') for x in response.get('result',{}).get('content',[]))
    assert 'escapes project root' in text or 'outside' in text,text
    after_response=request({'jsonrpc':'2.0','id':46,'method':'tools/call','params':{'name':'ctx_workspace_get','arguments':{}}},25)
    after=json.loads(next(x['text'] for x in after_response['result']['content'] if x.get('type')=='text'))
    assert after['revision']==3 and after['additionalRoots']==[],after
    print('WORKSPACE_MUTATIONS','PASS',flush=True)

  print('TOOLS',names[:65],flush=True)
  for i,n in enumerate(['ctx_tree','ctx_search','ctx_read'],3):
   if n not in names:continue
   args={'path':'/fixture/repo-b/unique-b.txt'} if n=='ctx_read' else ({'path':'/fixture/repo-b'} if n=='ctx_tree' else {'query':'cobalt lantern','path':'/fixture/repo-b'})
   v=request({'jsonrpc':'2.0','id':i,'method':'tools/call','params':{'name':n,'arguments':args}},25)
   print('CALL',n,json.dumps(v,ensure_ascii=False)[:2200],flush=True)
   content=' '.join(part.get('text','') for part in v.get('result',{}).get('content',[]))
   if n=='ctx_search':
    if mode=='roots' or (mode=='extra' and not strict):
     assert '1 results from 1 indexed chunks' in content and 'cobalt lantern walnut b' in content, content
    else:
     assert 'outside' in content or 'escapes project root' in content, content
   if n=='ctx_read':
    assert ('cobalt lantern walnut b' in content)==(mode=='roots' if strict else mode!='default'),content

  for i, target in enumerate(['/fixture/repo-outside', '/fixture/repo-a/escape'], 10):
   v=request({'jsonrpc':'2.0','id':i,'method':'tools/call','params':{'name':'ctx_search','arguments':{'query':'cobalt lantern','path':target}}},25)
   print('SECURITY',target,json.dumps(v,ensure_ascii=False)[:1800],flush=True)
   content=' '.join(part.get('text','') for part in v.get('result',{}).get('content',[]))
   assert 'escapes project root' in content or 'outside the request authority' in content,content

finally:
 p.terminate()
 try:out,err=p.communicate(timeout=3)
 except subprocess.TimeoutExpired:p.kill();out,err=p.communicate()
 print('EXIT',p.returncode,'STDERR',err[-2200:],flush=True)
