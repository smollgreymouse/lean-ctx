#!/usr/bin/env python3
"""End-to-end regression for strict Workspace + Git worktree lifecycle."""

from __future__ import annotations

import json
import os
from pathlib import Path
import select
import subprocess
import tempfile
import time


BINARY = os.environ.get("LEANCTX_TEST_BIN")
if not BINARY:
    raise SystemExit("set LEANCTX_TEST_BIN to the development lean-ctx binary")


def run(cwd: Path, env: dict[str, str], *args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [BINARY, *args],
        cwd=cwd,
        env=env,
        text=True,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=30,
        check=False,
    )


def git(cwd: Path, *args: str) -> str:
    env = os.environ.copy()
    env.update(
        {
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_TERMINAL_PROMPT": "0",
            "GIT_OPTIONAL_LOCKS": "0",
            "GIT_AUTHOR_NAME": "Workspace Test",
            "GIT_AUTHOR_EMAIL": "workspace@example.invalid",
            "GIT_COMMITTER_NAME": "Workspace Test",
            "GIT_COMMITTER_EMAIL": "workspace@example.invalid",
        }
    )
    result = subprocess.run(
        ["git", *args],
        cwd=cwd,
        env=env,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=20,
        check=False,
    )
    if result.returncode != 0:
        raise AssertionError((args, result.stdout, result.stderr))
    return result.stdout.strip()


def rpc_process(cwd: Path, env: dict[str, str]):
    proc = subprocess.Popen(
        [BINARY, "mcp"],
        cwd=cwd,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
        bufsize=1,
    )
    serial = 0

    def rpc(method: str, params: dict, timeout: float = 30.0) -> dict:
        nonlocal serial
        serial += 1
        ident = serial
        assert proc.stdin is not None and proc.stdout is not None
        proc.stdin.write(
            json.dumps({"jsonrpc": "2.0", "id": ident, "method": method, "params": params})
            + "\n"
        )
        proc.stdin.flush()
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if proc.poll() is not None:
                raise AssertionError(f"MCP exited early: {proc.returncode}")
            ready, _, _ = select.select([proc.stdout], [], [], 0.5)
            if not ready:
                continue
            line = proc.stdout.readline()
            if not line:
                break
            message = json.loads(line)
            if message.get("id") == ident:
                return message
        raise AssertionError(f"timeout waiting for {method}")

    return proc, rpc


with tempfile.TemporaryDirectory(prefix="leanctx-workspace-lifecycle-") as raw:
    base = Path(raw)
    home = base / "home"
    data = base / "data"
    config = base / "config"
    primary = base / "primary"
    extra = base / "extra"
    for path in (home, data, config, primary, extra):
        path.mkdir(parents=True, exist_ok=True)

    env = os.environ.copy()
    env.update(
        {
            "HOME": str(home),
            "XDG_CONFIG_HOME": str(config),
            "LEAN_CTX_DATA_DIR": str(data),
            "LEAN_CTX_HEADLESS": "1",
        }
    )

    store = base / "workspace.json"
    created = run(primary, env, "workspace-admin", "create", str(store), str(primary))
    assert created.returncode == 0, (created.stdout, created.stderr)
    added = run(primary, env, "workspace-admin", "add", str(store), str(extra), "1")
    assert added.returncode == 0, (added.stdout, added.stderr)
    assert json.loads(added.stdout)["revision"] == 2

    (extra / "marker.txt").write_text("extra authority marker\n")

    strict_env = env.copy()
    strict_env.update(
        {
            "LEAN_CTX_STRICT_WORKSPACE": "1",
            "LEAN_CTX_WORKSPACE_FILE": str(store),
        }
    )
    proc, rpc = rpc_process(primary, strict_env)
    try:
        init = rpc(
            "initialize",
            {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "workspace-lifecycle-regression", "version": "1"},
            },
        )
        assert "result" in init, init
        assert proc.stdin is not None
        proc.stdin.write(
            json.dumps(
                {
                    "jsonrpc": "2.0",
                    "method": "notifications/initialized",
                    "params": {},
                }
            )
            + "\n"
        )
        proc.stdin.flush()

        before = rpc(
            "tools/call",
            {"name": "ctx_workspace_get", "arguments": {}},
        )
        assert "result" in before, before
        before_text = "\n".join(
            block.get("text", "")
            for block in before["result"].get("content", [])
            if block.get("type") == "text"
        )
        before_payload = json.loads(before_text)
        assert before_payload["additionalRoots"] == [str(extra)], before_payload
        assert before_payload["rootStatus"] == [{"path": str(extra), "status": "active"}], before_payload
        before_fingerprint = before_payload["authorityFingerprint"]

        # External filesystem lifecycle change: no Workspace mutation/revision
        # occurs, yet the next request must immediately lose this authority.
        (extra / "marker.txt").unlink()
        extra.rmdir()

        result = rpc(
            "tools/call",
            {"name": "ctx_workspace_get", "arguments": {}},
        )
        assert "result" in result, result
        text = "\n".join(
            block.get("text", "")
            for block in result["result"].get("content", [])
            if block.get("type") == "text"
        )
        payload = json.loads(text)
        assert payload["additionalRoots"] == [], payload
        assert payload["configuredAdditionalRoots"] == [str(extra)], payload
        assert payload["rootStatus"] == [{"path": str(extra), "status": "missing"}], payload
        assert payload["authorityFingerprint"] != before_fingerprint, payload
        assert payload["revision"] == before_payload["revision"] == 2, payload
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=5)

    removed = run(primary, env, "workspace-admin", "remove", str(store), str(extra), "2")
    assert removed.returncode == 0, (removed.stdout, removed.stderr)
    assert json.loads(removed.stdout)["revision"] == 3

    # Crash-consistency of the identity sidecar: during commit it may contain
    # both old and next revisions. Readers must select the record matching the
    # legacy Workspace JSON on either side of the JSON rename.
    journal_primary = base / "journal-primary"
    journal_primary.mkdir()
    journal_store = base / "journal-workspace.json"
    journal_created = run(
        journal_primary,
        env,
        "workspace-admin",
        "create",
        str(journal_store),
        str(journal_primary),
    )
    assert journal_created.returncode == 0, (journal_created.stdout, journal_created.stderr)
    journal_meta = journal_store.with_name(journal_store.name + ".meta")
    meta_payload = json.loads(journal_meta.read_text())
    assert meta_payload["schema_version"] == 1, meta_payload
    assert len(meta_payload["versions"]) == 1, meta_payload
    old_meta = meta_payload["versions"][0]
    next_meta = json.loads(json.dumps(old_meta))
    next_meta["revision"] = 2
    journal_meta.write_text(
        json.dumps(
            {
                "schema_version": 1,
                "versions": [old_meta, next_meta],
            }
        )
    )
    before_json_commit = run(
        journal_primary,
        env,
        "workspace-admin",
        "show",
        str(journal_store),
    )
    assert before_json_commit.returncode == 0, before_json_commit.stderr
    assert json.loads(before_json_commit.stdout)["revision"] == 1

    store_payload = json.loads(journal_store.read_text())
    store_payload["revision"] = 2
    journal_store.write_text(json.dumps(store_payload))
    after_json_commit = run(
        journal_primary,
        env,
        "workspace-admin",
        "show",
        str(journal_store),
    )
    assert after_json_commit.returncode == 0, after_json_commit.stderr
    assert json.loads(after_json_commit.stdout)["revision"] == 2

    repo = base / "repo"
    git(base, "init", str(repo))
    (repo / "tracked.txt").write_text("main\n")
    git(repo, "add", "tracked.txt")
    git(repo, "commit", "-m", "initial")
    git(repo, "branch", "feature-a")
    linked = base / "linked"
    git(repo, "worktree", "add", str(linked), "feature-a")

    # A sibling checkout of the same local repository is topology metadata,
    # never an independent additional Workspace authority root.
    main_store = base / "main-workspace.json"
    created_main = run(repo, env, "workspace-admin", "create", str(main_store), str(repo))
    assert created_main.returncode == 0, (created_main.stdout, created_main.stderr)
    sibling_add = run(
        repo,
        env,
        "workspace-admin",
        "add",
        str(main_store),
        str(linked),
        "1",
    )
    assert sibling_add.returncode != 0, (sibling_add.stdout, sibling_add.stderr)
    sibling_error = (sibling_add.stderr + sibling_add.stdout).lower()
    assert "worktree" in sibling_error and "independent workspace authority" in sibling_error, sibling_error

    git_store = base / "git-workspace.json"
    created = run(linked, env, "workspace-admin", "create", str(git_store), str(linked))
    assert created.returncode == 0, (created.stdout, created.stderr)
    original = json.loads(created.stdout)
    binding = original["checkout_bindings"][str(linked)]
    assert binding["checkout_key"] and binding["git_dir"]

    moved = base / "linked-moved"
    git(repo, "worktree", "move", str(linked), str(moved))
    moved_env = strict_env.copy()
    moved_env["LEAN_CTX_WORKSPACE_FILE"] = str(git_store)
    proc, rpc = rpc_process(moved, moved_env)
    try:
        init = rpc(
            "initialize",
            {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "workspace-lifecycle-regression", "version": "1"},
            },
        )
        assert "result" in init, init
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=5)

    # A removed primary worktree must not kill the MCP protocol surface.
    # Start from another live cwd: the launcher-bound store remains the only
    # authority source, so diagnostics work while content tools fail closed.
    git(repo, "worktree", "remove", str(moved))
    proc, rpc = rpc_process(repo, moved_env)
    try:
        init = rpc(
            "initialize",
            {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "workspace-lifecycle-regression", "version": "1"},
            },
        )
        assert "result" in init, init
        assert proc.stdin is not None
        proc.stdin.write(
            json.dumps(
                {
                    "jsonrpc": "2.0",
                    "method": "notifications/initialized",
                    "params": {},
                }
            )
            + "\n"
        )
        proc.stdin.flush()
        diagnosed = rpc(
            "tools/call",
            {"name": "ctx_workspace_diagnose", "arguments": {}},
        )
        text = "\n".join(
            block.get("text", "")
            for block in diagnosed.get("result", {}).get("content", [])
            if block.get("type") == "text"
        )
        payload = json.loads(text)
        assert payload["status"] == "launcher_bound_degraded", payload
        assert payload["workspace"]["primaryRoot"] is None, payload
        assert payload["workspace"]["primaryStatus"] == "missing", payload

        denied = rpc(
            "tools/call",
            {
                "name": "ctx_read",
                "arguments": {"path": str(repo / "tracked.txt"), "fresh": True},
            },
        )
        assert "error" in denied, denied
        assert "authority" in denied["error"]["message"].lower(), denied
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=5)

    # Reusing the old filesystem path for a newly-created worktree must not
    # resurrect the old checkout authority. The generation/identity mismatch is
    # diagnosable, while content tools remain denied.
    git(repo, "worktree", "add", str(linked), "feature-a")
    proc, rpc = rpc_process(linked, moved_env)
    try:
        init = rpc(
            "initialize",
            {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "workspace-lifecycle-regression", "version": "1"},
            },
        )
        assert "result" in init, init
        assert proc.stdin is not None
        proc.stdin.write(
            json.dumps(
                {
                    "jsonrpc": "2.0",
                    "method": "notifications/initialized",
                    "params": {},
                }
            )
            + "\n"
        )
        proc.stdin.flush()
        diagnosed = rpc(
            "tools/call",
            {"name": "ctx_workspace_diagnose", "arguments": {}},
        )
        text = "\n".join(
            block.get("text", "")
            for block in diagnosed.get("result", {}).get("content", [])
            if block.get("type") == "text"
        )
        payload = json.loads(text)
        assert payload["status"] == "launcher_bound_degraded", payload
        assert payload["workspace"]["primaryRoot"] is None, payload
        assert payload["workspace"]["primaryStatus"] == "identity-mismatch", payload
        denied = rpc(
            "tools/call",
            {
                "name": "ctx_read",
                "arguments": {"path": str(linked / "tracked.txt"), "fresh": True},
            },
        )
        assert "error" in denied, denied
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=5)

print("PASS: Workspace missing/remove/move/remove-primary/recreate lifecycle")
