//! The token an operator-started `ahma serve unix` or `ahma serve http`
//! requires by default (SPEC R-HUB.14).
//!
//! These servers are reachable by the user's own sandboxed commands exactly as
//! the hub is — a unix socket outside any rule, a loopback TCP port the
//! sandbox's network allowance covers — and a session on them locks the scope
//! its client's `roots/list` names. So, unless the operator configured a token
//! or explicitly chose none, the server makes one: 64 hex characters, written
//! owner-only to a file ahma's own clients read, never printed or logged.

use crate::shell::cli::AppConfig;
use ahma_common::hub::{HubToken, publish_token, serve_http_token_path, token_path_for};
use ahma_http_bridge::BridgeConfig;
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;

/// Where a server publishes the token it generated.
pub(super) enum Home {
    /// Beside the unix socket it serves (`mcp.sock` → `mcp.token`), where the
    /// clients that connect to that socket already look.
    Socket(PathBuf),
    /// In the runtime directory, named for the TCP port, where a client of a
    /// loopback `http://` URL looks.
    Port(u16),
}

/// What the server does about authentication, and the token file it owns.
/// Dropping it removes the file.
pub(super) struct BridgeAuth {
    published: Arc<parking_lot::Mutex<Option<PathBuf>>>,
}

impl Drop for BridgeAuth {
    fn drop(&mut self) {
        if let Some(path) = self.published.lock().take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Decide the server's authentication and apply it to `bridge`.
///
/// - A token or token file the operator configured is used as given, and
///   nothing is generated or published.
/// - `--allow-unauthenticated` runs without one, and says so on every start:
///   it is the operator's choice, never a silent default (SPEC R7).
/// - Otherwise a token is generated and published at `home`.
pub(super) async fn apply(
    config: &AppConfig,
    home: Home,
    bridge: &mut BridgeConfig,
) -> Result<BridgeAuth> {
    let published = Arc::new(parking_lot::Mutex::new(None));
    let auth = BridgeAuth {
        published: published.clone(),
    };

    if config.require_token.is_some() || config.require_token_path.is_some() {
        return Ok(auth);
    }
    if config.allow_unauthenticated {
        tracing::warn!(
            "UNAUTHENTICATED: this server requires no token (--allow-unauthenticated). Any \
             process on this machine that can reach it — including a command running inside \
             ahma's sandbox — can open a session and choose its own sandbox scope."
        );
        eprintln!(
            "ahma: UNAUTHENTICATED server (--allow-unauthenticated): any local process, \
             including a sandboxed command, can use it."
        );
        return Ok(auth);
    }

    let token = HubToken::generate();
    bridge.require_token = Some(token.as_str().to_owned());
    match home {
        Home::Socket(socket) => {
            // An abstract socket's token lives in the runtime directory: make
            // sure it exists, owner-only, as the hub does, before writing there.
            let _ = ahma_common::hub::runtime_dir();
            let path = token_path_for(&socket);
            // Recorded first, so a drop at any point removes what was written.
            *published.lock() = Some(path.clone());
            publish_token(&path, &token).await.with_context(|| {
                format!("cannot publish the server's token at {}", path.display())
            })?;
        }
        Home::Port(port) if port != 0 => {
            let path = serve_http_token_path(port).context("no runtime directory for the token")?;
            *published.lock() = Some(path.clone());
            publish_token(&path, &token).await.with_context(|| {
                format!("cannot publish the server's token at {}", path.display())
            })?;
        }
        Home::Port(_) => {
            // Port 0: the name is the port the bind picks. Published the moment
            // it is known; a client that asks before then is refused once and
            // reads the file on its retry.
            let (tx, rx) = tokio::sync::oneshot::channel::<u16>();
            bridge.bound_port_tx = Some(tx);
            let published = published.clone();
            tokio::spawn(async move {
                let Ok(port) = rx.await else { return };
                let Some(path) = serve_http_token_path(port) else {
                    return;
                };
                *published.lock() = Some(path.clone());
                if let Err(e) = publish_token(&path, &token).await {
                    tracing::error!("cannot publish the server's token: {e}");
                }
            });
        }
    }
    Ok(auth)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bridge() -> BridgeConfig {
        BridgeConfig::default()
    }

    /// An operator's own token is used as given: nothing is generated, and no
    /// file appears for a client to find.
    #[tokio::test]
    async fn a_configured_token_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("mcp.sock");
        let config = AppConfig {
            require_token: Some("operators-own".into()),
            ..AppConfig::default()
        };
        let mut b = bridge();
        b.require_token = Some("operators-own".into());
        let _auth = apply(&config, Home::Socket(socket.clone()), &mut b)
            .await
            .unwrap();
        assert_eq!(b.require_token.as_deref(), Some("operators-own"));
        assert!(!token_path_for(&socket).exists());

        let config = AppConfig {
            require_token_path: Some(dir.path().join("t")),
            ..AppConfig::default()
        };
        let mut b = bridge();
        let _auth = apply(&config, Home::Socket(socket.clone()), &mut b)
            .await
            .unwrap();
        assert!(
            b.require_token.is_none(),
            "the bridge loads the file itself"
        );
        assert!(!token_path_for(&socket).exists());
    }

    /// Choosing no token is allowed and never silent: the server runs open
    /// and publishes nothing.
    #[tokio::test]
    async fn the_opt_out_runs_open_and_publishes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("mcp.sock");
        let config = AppConfig {
            allow_unauthenticated: true,
            ..AppConfig::default()
        };
        let mut b = bridge();
        let _auth = apply(&config, Home::Socket(socket.clone()), &mut b)
            .await
            .unwrap();
        assert!(b.require_token.is_none());
        assert!(!token_path_for(&socket).exists());
    }

    /// The default: a token the server makes, written owner-only beside its
    /// socket, required by the bridge, and gone when the server is.
    #[tokio::test]
    async fn by_default_a_unix_server_publishes_and_requires_its_own_token() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("mcp.sock");
        let mut b = bridge();
        let auth = apply(&AppConfig::default(), Home::Socket(socket.clone()), &mut b)
            .await
            .unwrap();

        let required = b.require_token.clone().expect("a token is required");
        assert_eq!(required.len(), 64);
        let file = token_path_for(&socket);
        assert_eq!(std::fs::read_to_string(&file).unwrap().trim(), required);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "owner-only");
        }
        drop(auth);
        assert!(!file.exists(), "removed with the server");
    }

    /// A TCP server's token is named for its port, in the runtime directory.
    #[tokio::test]
    async fn by_default_an_http_server_publishes_a_token_for_its_port() {
        let mut b = bridge();
        let auth = apply(&AppConfig::default(), Home::Port(48123), &mut b)
            .await
            .unwrap();
        let file = serve_http_token_path(48123).expect("a runtime dir");
        let required = b.require_token.clone().expect("a token is required");
        assert_eq!(std::fs::read_to_string(&file).unwrap().trim(), required);
        drop(auth);
        assert!(!file.exists());
    }

    /// Port 0 is named for the port the bind picks, once it is known.
    #[tokio::test]
    async fn an_http_server_on_port_zero_publishes_once_the_port_is_known() {
        let mut b = bridge();
        let auth = apply(&AppConfig::default(), Home::Port(0), &mut b)
            .await
            .unwrap();
        let tx = b.bound_port_tx.take().expect("the bridge reports its port");
        tx.send(48124).unwrap();
        let file = serve_http_token_path(48124).expect("a runtime dir");
        let deadline =
            std::time::Instant::now() + ahma_common::timeouts::TestTimeouts::scale_secs(5);
        while !file.exists() {
            assert!(std::time::Instant::now() < deadline, "never published");
            tokio::time::sleep(ahma_common::timeouts::TestTimeouts::poll_interval()).await;
        }
        drop(auth);
        assert!(!file.exists());
    }
}
