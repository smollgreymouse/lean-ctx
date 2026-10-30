#!/usr/bin/env python3
"""Black-box regression: ctx_tools discovery owns its downstream timeout.

The generic tool watchdog must never cancel ctx_tools while a healthy downstream
MCP is still within gateway.call_timeout_secs. The fixture intentionally makes
tools/list slower than LEAN_CTX_TOOL_TIMEOUT_SECS.
"""
from __future__ import annotations

import json
import os
from pathlib import Path
import select
import subprocess
import tempfile
import textwrap
import time

BINARY = Path(os.environ["LEANCTX_TEST_BIN"]).resolve()

FAKE_SERVER = r'''#!/usr/bin/env python3
import json, sys, time

for raw in sys.stdin:
    try:
        req = json.loads(raw)
    except Exception:
        continue
    method = req.get("method")
    ident = req.get("id")
    if method == "initialize":
        result = {
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "slow-fixture", "version": "1"},
        }
    elif method == "tools/list":
        time.sleep(11.5)
        result = {
            "tools": [{
                "name": "echo",
                "description": "Echo fixture text",
                "inputSchema": {
                    "type": "object",
                    "properties": {"text": {"type": "string"}},
                },
            }]
        }
    elif method == "tools/call":
        text = (req.get("params") or {}).get("arguments", {}).get("text", "")
        result = {
            "content": [{"type": "text", "text": "echo:" + str(text)}],
            "isError": False,
        }
    else:
        if ident is None:
            continue
        result = {}
    if ident is not None:
        sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": ident, "result": result}) + "\n")
        sys.stdout.flush()
'''


def rpc_process(cwd: Path, env: dict[str, str]):
    proc = subprocess.Popen(
        [str(BINARY), "mcp"],
        cwd=cwd,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
    )
    serial = 0

    def rpc(method: str, params: dict, timeout: float = 45.0) -> dict:
        nonlocal serial
        serial += 1
        ident = serial
        assert proc.stdin is not None and proc.stdout is not None
        proc.stdin.write(json.dumps({
            "jsonrpc": "2.0", "id": ident, "method": method, "params": params
        }) + "\n")
        proc.stdin.flush()
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if proc.poll() is not None:
                err = proc.stderr.read() if proc.stderr is not None else ""
                raise AssertionError(f"MCP exited early rc={proc.returncode}: {err[-4000:]}")
            ready, _, _ = select.select([proc.stdout], [], [], 0.5)
            if not ready:
                continue
            line = proc.stdout.readline()
            if not line:
                break
            msg = json.loads(line)
            if msg.get("id") == ident:
                return msg
        raise AssertionError(f"timeout waiting for {method}")

    return proc, rpc


def call(rpc, args: dict, timeout: float = 45.0) -> tuple[dict, float]:
    started = time.monotonic()
    reply = rpc("tools/call", {"name": "ctx_tools", "arguments": args}, timeout)
    return reply, time.monotonic() - started


def reply_text(reply: dict) -> str:
    if "error" in reply:
        return json.dumps(reply["error"])
    return "\n".join(
        block.get("text", "")
        for block in reply.get("result", {}).get("content", [])
        if block.get("type") == "text"
    )


with tempfile.TemporaryDirectory(prefix="leanctx-ctx-tools-watchdog-") as raw:
    base = Path(raw)
    home = base / "home"
    project = base / "project"
    config_dir = base / "config" / "lean-ctx"
    data = base / "data"
    for path in (home, project, config_dir, data):
        path.mkdir(parents=True, exist_ok=True)

    fake = base / "slow_mcp.py"
    fake.write_text(FAKE_SERVER)
    fake.chmod(0o755)

    (config_dir / "config.toml").write_text(textwrap.dedent(f"""
        shadow_mode = true
        permission_inheritance = "off"
        rules_injection = "off"
        minimal_overhead = true
        tools_enabled = ["ctx_tools"]
        default_tool_categories = ["core"]

        [gateway]
        enabled = true
        cache_ttl_secs = 300
        call_timeout_secs = 20
        top_n = 5

        [[gateway.servers]]
        name = "slow"
        transport = "stdio"
        enabled = true
        command = "/usr/bin/python3"
        args = ["{fake}"]
    """).strip() + "\n")

    env = os.environ.copy()
    env.update({
        "HOME": str(home),
        "XDG_CONFIG_HOME": str(base / "config"),
        "LEAN_CTX_CONFIG_DIR": str(config_dir),
        "LEAN_CTX_DATA_DIR": str(data),
        "LEAN_CTX_HEADLESS": "1",
        # Deliberately shorter than the downstream tools/list latency.
        "LEAN_CTX_TOOL_TIMEOUT_SECS": "10",
    })

    proc, rpc = rpc_process(project, env)
    try:
        init = rpc("initialize", {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "ctx-tools-watchdog-regression", "version": "1"},
        })
        assert "result" in init, init
        assert proc.stdin is not None
        proc.stdin.write(json.dumps({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {},
        }) + "\n")
        proc.stdin.flush()

        first, first_elapsed = call(rpc, {"action": "find", "query": "echo"})
        assert "error" not in first, first
        first_text = reply_text(first)
        assert "slow::echo" in first_text, first_text
        assert first_elapsed >= 10.5, first_elapsed

        direct, direct_elapsed = call(
            rpc,
            {"action": "call", "tool": "slow::echo", "arguments": {"text": "ok"}},
        )
        assert "error" not in direct, direct
        assert "echo:ok" in reply_text(direct), direct
        assert direct_elapsed < 10.0, direct_elapsed

        refreshed, refresh_elapsed = call(rpc, {"action": "refresh"})
        assert "error" not in refreshed, refreshed
        refresh_text = reply_text(refreshed)
        assert "gateway catalog refreshed" in refresh_text, refresh_text
        assert "slow::echo" in refresh_text, refresh_text
        assert refresh_elapsed >= 10.5, refresh_elapsed

        after, after_elapsed = call(rpc, {"action": "find", "query": "echo"})
        assert "error" not in after, after
        assert "slow::echo" in reply_text(after), after
        assert after_elapsed < 10.0, after_elapsed

        print(
            "PASS: ctx_tools find/refresh survive generic watchdog "
            f"(find={first_elapsed:.2f}s refresh={refresh_elapsed:.2f}s)"
        )
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=5)
