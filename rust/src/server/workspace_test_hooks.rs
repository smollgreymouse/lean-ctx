// SPDX-License-Identifier: Apache-2.0
//! Debug-only deterministic synchronization hooks for workspace security tests.
#![cfg(debug_assertions)]
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Default)]
struct HookState {
    armed: Option<&'static str>,
    reached: bool,
    released: bool,
    hits: usize,
}

fn state() -> &'static (Mutex<HookState>, Condvar) {
    static STATE: OnceLock<(Mutex<HookState>, Condvar)> = OnceLock::new();
    STATE.get_or_init(|| (Mutex::new(HookState::default()), Condvar::new()))
}

pub(crate) fn arm(stage: &'static str) {
    let (lock, _) = state();
    let mut guard = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = HookState {
        armed: Some(stage),
        reached: false,
        released: false,
        hits: 0,
    };
}

pub(crate) fn hit(stage: &'static str) {
    let (lock, cv) = state();
    let mut guard = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if guard.armed != Some(stage) {
        return;
    }
    guard.hits += 1;
    guard.reached = true;
    cv.notify_all();
    while !guard.released {
        guard = cv
            .wait(guard)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
}

pub(crate) fn wait_hits(expected: usize, timeout: Duration) -> bool {
    let (lock, cv) = state();
    let deadline = Instant::now() + timeout;
    let mut guard = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    while guard.hits < expected {
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        let (next, wait) = cv
            .wait_timeout(guard, deadline.saturating_duration_since(now))
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard = next;
        if wait.timed_out() && guard.hits < expected {
            return false;
        }
    }
    true
}

pub(crate) fn wait_reached(timeout: Duration) -> bool {
    let (lock, cv) = state();
    let deadline = Instant::now() + timeout;
    let mut guard = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    while !guard.reached {
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        let (next, wait) = cv
            .wait_timeout(guard, deadline.saturating_duration_since(now))
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard = next;
        if wait.timed_out() && !guard.reached {
            return false;
        }
    }
    true
}

pub(crate) fn release() {
    let (lock, cv) = state();
    let mut guard = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.released = true;
    cv.notify_all();
}

pub(crate) fn clear() {
    let (lock, cv) = state();
    let mut guard = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.released = true;
    guard.armed = None;
    cv.notify_all();
}

/// Cross-process debug barrier for admin CLI crash/race tests. The controller
/// chooses exactly one stage and releases it by creating `<stage>.release`.
pub(crate) fn external_hit(stage: &'static str) {
    let Ok(selected) = std::env::var("LEAN_CTX_TEST_HOOK_STAGE") else {
        return;
    };
    if selected != stage {
        return;
    }
    let Ok(dir) = std::env::var("LEAN_CTX_TEST_HOOK_DIR") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let ready = dir.join(format!("{stage}.ready"));
    let release = dir.join(format!("{stage}.release"));
    let _ = std::fs::write(&ready, b"ready");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !release.exists() {
        assert!(
            Instant::now() < deadline,
            "workspace test hook timed out: {stage}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
