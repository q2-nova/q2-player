//! The negative control of the clippy step in .github/workflows/lint.yml.
//! Clippy must report every lint denied here, with the lists of the
//! repository's clippy.toml, or the step that lints q2nd could pass without
//! enforcing them. Each function is one mistake that step exists for.
#![deny(clippy::await_holding_lock, clippy::await_holding_refcell_ref)]

pub mod converted {
    #![deny(clippy::disallowed_methods, clippy::disallowed_types)]

    /// A sleep on the executor.
    pub async fn sleeps() {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }

    /// A blocking socket in an async module.
    pub fn takes_a_blocking_socket(_: std::os::unix::net::UnixDatagram) {}
}

async fn tick() {}

/// A std lock held across an await.
pub async fn holds_a_lock(shared: &std::sync::Mutex<u32>) {
    let guard = shared.lock().unwrap();
    tick().await;
    drop(guard);
}

/// A RefCell borrow held across an await.
pub async fn holds_a_borrow(shared: &std::cell::RefCell<u32>) {
    let value = shared.borrow_mut();
    tick().await;
    drop(value);
}
