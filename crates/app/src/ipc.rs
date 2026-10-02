//! The single-instance lock doubles as a tiny control channel.
//!
//! The running app binds the abstract Unix datagram socket
//! `@f9-talk-instance-lock`; a second bind fails, which is how a second
//! launch knows one is already running. The Settings window sends
//! `reload` to that socket after Save, and the running app rebuilds its
//! speech-to-text backend from the new settings and keys, with no
//! restart.

use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixDatagram};

const INSTANCE_LOCK: &[u8] = b"f9-talk-instance-lock";
const SETTINGS_LOCK: &[u8] = b"f9-talk-settings-lock";
pub const RELOAD: &[u8] = b"reload";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// Settings were saved: rebuild the backend.
    Reload,
}

/// Bind the instance lock. `Err` means another f9-talk is running.
pub fn acquire_instance_lock() -> std::io::Result<UnixDatagram> {
    bind(INSTANCE_LOCK)
}

/// At most one Settings window: `Err` means one is already open.
pub fn acquire_settings_lock() -> std::io::Result<UnixDatagram> {
    bind(SETTINGS_LOCK)
}

fn bind(name: &[u8]) -> std::io::Result<UnixDatagram> {
    UnixDatagram::bind_addr(&SocketAddr::from_abstract_name(name)?)
}

/// Tell the running app to reload its settings. `false` when no f9-talk
/// is running.
pub fn notify_reload() -> bool {
    let Ok(sock) = UnixDatagram::unbound() else {
        return false;
    };
    let Ok(addr) = SocketAddr::from_abstract_name(INSTANCE_LOCK) else {
        return false;
    };
    sock.send_to_addr(RELOAD, &addr).is_ok()
}

/// Read control messages from the bound lock socket on a thread and
/// forward them to the session loop.
pub fn spawn_listener(lock: &UnixDatagram, tx: tokio::sync::mpsc::Sender<Control>) {
    let Ok(sock) = lock.try_clone() else {
        tracing::warn!("could not listen for settings changes (socket clone failed)");
        return;
    };
    std::thread::Builder::new()
        .name("f9-talk-control".into())
        .spawn(move || {
            let mut buf = [0u8; 64];
            while let Ok(n) = sock.recv(&mut buf) {
                if &buf[..n] == RELOAD && tx.blocking_send(Control::Reload).is_err() {
                    return;
                }
            }
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reload_reaches_the_lock_holder() {
        // A private name so the test never talks to a real instance.
        let name = format!("f9-talk-ipc-test-{}", std::process::id());
        let lock = bind(name.as_bytes()).unwrap();
        assert!(bind(name.as_bytes()).is_err(), "second bind must fail");
        let addr = SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        UnixDatagram::unbound()
            .unwrap()
            .send_to_addr(RELOAD, &addr)
            .unwrap();
        let mut buf = [0u8; 16];
        let n = lock.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], RELOAD);
    }
}
