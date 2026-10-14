#!/usr/bin/env python3
"""Standalone real-MCP PathJail/search authority regression, Linux bubblewrap only.

Run with LEANCTX_TEST_BIN=/absolute/path/to/real/lean-ctx and one of
`default`, `allow`, `extra`. No production config or projects are mounted.
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
assert mode in ('default','allow','extra')
if mode=='extra':cmd+=['--setenv','LEAN_CTX_EXTRA_ROOTS','/fixture/repo-b']
if mode=='allow':cmd+=['--setenv','LEAN_CTX_ALLOW_PATH','/fixture/repo-b']
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
   if msg.get('id')==obj['id']: return msg
  except Exception: pass
 return {'error':'timeout or closed','returncode':p.poll()}
try:
 init=request({'jsonrpc':'2.0','id':1,'method':'initialize','params':{'protocolVersion':'2025-06-18','capabilities':{},'clientInfo':{'name':'isolated-baseline','version':'1'}}},30)
 print('INIT',str(init)[:1000],flush=True)
 if 'result' in init:
  p.stdin.write(json.dumps({'jsonrpc':'2.0','method':'notifications/initialized','params':{}})+'\n');p.stdin.flush()
  tools=request({'jsonrpc':'2.0','id':2,'method':'tools/list','params':{}},18)
  names=[t.get('name') for t in tools.get('result',{}).get('tools',[])]
  print('TOOLS',names[:65],flush=True)
  for i,n in enumerate(['ctx_tree','ctx_search','ctx_read'],3):
   if n not in names:continue
   args={'path':'/fixture/repo-b/unique-b.txt'} if n=='ctx_read' else ({'path':'/fixture/repo-b'} if n=='ctx_tree' else {'query':'cobalt lantern','path':'/fixture/repo-b'})
   v=request({'jsonrpc':'2.0','id':i,'method':'tools/call','params':{'name':n,'arguments':args}},25)
   print('CALL',n,json.dumps(v,ensure_ascii=False)[:2200],flush=True)
   content=' '.join(part.get('text','') for part in v.get('result',{}).get('content',[]))
   if n=='ctx_search':
    if mode=='extra':
     assert '1 results from 1 indexed chunks' in content and 'cobalt lantern walnut b' in content, content
    else:
     assert 'outside' in content or 'escapes project root' in content, content
   if n=='ctx_read':
    assert ('cobalt lantern walnut b' in content)==(mode!='default'),content

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
