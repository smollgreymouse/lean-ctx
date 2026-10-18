#!/usr/bin/env python3
import json, os, pathlib, signal, subprocess, tempfile, time, shutil

binary = os.environ['LEANCTX_TEST_BIN']
base = pathlib.Path(tempfile.mkdtemp(prefix='leanctx-store-race-'))

def run(*args, env=None, check=True):
    p = subprocess.run([binary,'workspace-admin',*map(str,args)], text=True,
                       capture_output=True, env=env, timeout=30)
    if check and p.returncode != 0:
        raise AssertionError((args,p.returncode,p.stdout,p.stderr))
    return p

def mkfixture(tag):
    d=base/tag; root=d/'root'; extra=d/'extra'; state=d/'state'; hooks=d/'hooks'
    for x in (root,extra,state,hooks): x.mkdir(parents=True,exist_ok=True)
    (root/'.git').mkdir(); (extra/'.git').mkdir()
    store=state/'workspace.json'
    run('create',store,root)
    return root,extra,store,hooks

def wait_ready(hooks,stage,proc):
    ready=hooks/f'{stage}.ready'; deadline=time.monotonic()+20
    while time.monotonic()<deadline:
        if ready.exists(): return
        if proc.poll() is not None: raise AssertionError((stage,'process exited',proc.returncode,proc.stderr.read()))
        time.sleep(.01)
    raise AssertionError((stage,'timeout'))

def hooked_admin(stage,hooks,*args):
    env=os.environ.copy(); env['LEAN_CTX_TEST_HOOK_STAGE']=stage; env['LEAN_CTX_TEST_HOOK_DIR']=str(hooks)
    return subprocess.Popen([binary,'workspace-admin',*map(str,args)], text=True,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)

def show(store):
    return json.loads(run('show',store).stdout)

try:
    # Crash before rename: old authority remains valid; orphan temp is ignored.
    root,extra,store,hooks=mkfixture('before-rename')
    p=hooked_admin('store_before_rename',hooks,'add',store,extra,1)
    wait_ready(hooks,'store_before_rename',p); p.kill(); p.wait(timeout=5)
    state=show(store)
    assert state['revision']==1 and str(extra) not in state['additional'],state
    assert any(x.name.startswith('.workspace.json.') and x.name.endswith('.tmp') for x in store.parent.iterdir()), list(store.parent.iterdir())
    print('PASS: crash before rename preserves old valid Workspace state')

    # Crash after atomic rename but before parent fsync: process-level crash leaves valid new JSON.
    root,extra,store,hooks=mkfixture('after-rename')
    p=hooked_admin('store_after_rename_before_dir_fsync',hooks,'add',store,extra,1)
    wait_ready(hooks,'store_after_rename_before_dir_fsync',p); p.kill(); p.wait(timeout=5)
    state=show(store)
    assert state['revision']==2 and str(extra) in state['additional'],state
    print('PASS: crash after rename leaves parseable monotonic Workspace state')

    # Parent path replacement while operation is blocked before open.
    d=base/'parent-race'; root=d/'root'; safe=d/'safe'; evil=d/'evil'; hooks=d/'hooks'
    for x in (root,safe,evil,hooks): x.mkdir(parents=True,exist_ok=True)
    (root/'.git').mkdir()
    store=safe/'workspace.json'
    p=hooked_admin('store_before_open',hooks,'create',store,root)
    wait_ready(hooks,'store_before_open',p)
    safe_old=d/'safe-old'; safe.rename(safe_old); safe.symlink_to(evil, target_is_directory=True)
    (hooks/'store_before_open.release').write_text('go')
    out,err=p.communicate(timeout=10)
    vulnerable=(evil/'workspace.json').exists() and p.returncode==0
    if vulnerable:
        print('FAIL: parent symlink swap redirected Workspace state write')
        raise SystemExit(3)
    assert not (evil/'workspace.json').exists(), (out,err)
    print('PASS: parent path swap cannot redirect Workspace state')
finally:
    shutil.rmtree(base,ignore_errors=True)
