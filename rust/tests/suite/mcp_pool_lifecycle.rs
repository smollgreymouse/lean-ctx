// SPDX-License-Identifier: Apache-2.0

//! Downstream MCP transport diagnostics and runtime lifecycle.
//!
//! The gateway pool logs transport-closed evictions, idle-TTL expiry,
//! concurrent connection replacement, and post-failure evictions (ported from
//! `fix/gateway-lifecycle-diagnostics`). This module pins the lifecycle those
//! logs describe: the proxy runtime must release its port on shutdown and
//! restart cleanly on the same port — a leaked listener or a half-closed
//! runtime makes every subsequent gateway start fail.

use std::time::Duration;

/// Reserve an ephemeral loopback port, then release it for the proxy to bind.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe socket");
    let port = listener.local_addr().expect("probe local_addr").port();
    drop(listener);
    port
}

async fn wait_healthy(client: &reqwest::Client, base: &str) -> bool {
    for _ in 0..50 {
        if let Ok(resp) = client.get(format!("{base}/health")).send().await
            && resp.status().is_success()
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

async fn wait_port_released(port: u16) -> bool {
    for _ in 0..50 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

fn spawn_proxy(port: u16, token: String) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let _ = lean_ctx::proxy::start_proxy_with_token(port, Some(token)).await;
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_port_is_released_after_shutdown_and_restart_succeeds() {
    let port = free_port();
    let token = "lifecycle-token".to_string();
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();

    // First runtime: bind → serve → health.
    let first = spawn_proxy(port, token.clone());
    assert!(
        wait_healthy(&client, &base).await,
        "first proxy runtime never became ready"
    );

    // Shutdown: the listener must actually go away within the deadline, not
    // linger as a leaked socket that blocks the next start.
    first.abort();
    assert!(
        wait_port_released(port).await,
        "proxy port still accepting connections after shutdown"
    );

    // Restart on the same port: the regression this pins is a clean second
    // lifecycle after the first one tore down (gateway restart path).
    let second = spawn_proxy(port, token);
    assert!(
        wait_healthy(&client, &base).await,
        "restarted proxy runtime never became ready"
    );
    second.abort();
    assert!(
        wait_port_released(port).await,
        "restarted proxy port still accepting after shutdown"
    );
}
