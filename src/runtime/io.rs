use std::io;
use std::sync::Arc;
use std::time::Duration;

use socket2::{SockRef, Socket, TcpKeepalive};
use tokio::net::TcpSocket;
use tracing::warn;

/// Hook trait used by [`SocketConfigurator`] to apply platform-specific options.
pub trait SocketConfigHook: Send + Sync {
    fn configure_tcp_client(&self, socket: &TcpSocket) -> io::Result<()>;
    fn configure_raw_socket(&self, socket: &Socket) -> io::Result<()>;
}

#[derive(Default)]
struct SystemSocketConfig;

impl SocketConfigHook for SystemSocketConfig {
    fn configure_tcp_client(&self, socket: &TcpSocket) -> io::Result<()> {
        socket.set_reuseaddr(true)?;
        socket.set_nodelay(true)?;

        let sock_ref = SockRef::from(socket);
        let _ = sock_ref.set_keepalive(true);

        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            let keepalive = TcpKeepalive::new().with_time(Duration::from_secs(30));
            let _ = sock_ref.set_tcp_keepalive(&keepalive);
        }

        Ok(())
    }

    fn configure_raw_socket(&self, socket: &Socket) -> io::Result<()> {
        socket.set_nonblocking(true)?;
        Ok(())
    }
}

/// Helper responsible for applying socket configuration and capturing failures.
#[derive(Clone)]
pub struct SocketConfigurator {
    hook: Arc<dyn SocketConfigHook>,
}

impl SocketConfigurator {
    pub fn new() -> Self {
        Self {
            hook: Arc::new(SystemSocketConfig),
        }
    }

    pub fn with_hook<H>(hook: H) -> Self
    where
        H: SocketConfigHook + 'static,
    {
        Self {
            hook: Arc::new(hook),
        }
    }

    pub fn configure_tcp_client(&self, socket: &TcpSocket) {
        if let Err(err) = self.hook.configure_tcp_client(socket) {
            metrics::counter!("socket.config_errors").increment(1);
            warn!(error = %err, "failed to configure TCP socket");
        }
    }

    pub fn configure_raw_socket(&self, socket: &Socket) {
        if let Err(err) = self.hook.configure_raw_socket(socket) {
            metrics::counter!("socket.config_errors").increment(1);
            warn!(error = %err, "failed to configure raw socket");
        }
    }
}

impl Default for SocketConfigurator {
    fn default() -> Self {
        Self::new()
    }
}
