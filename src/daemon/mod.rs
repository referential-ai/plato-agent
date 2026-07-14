pub mod client;
#[cfg(windows)]
pub mod control;
mod handlers;
#[cfg(windows)]
pub mod installer_gate;
pub mod lock;
pub mod protocol;
mod runtime;
pub mod server;
pub(crate) mod transport;

pub fn wake_listener(endpoint: &std::path::Path) {
    transport::wake(endpoint);
}
