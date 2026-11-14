//! `ahma gui`: the browser as a second surface over the same session as the
//! terminal (SPEC R-GUI).
//!
//! The server owns everything that decides anything — what a human is shown,
//! what they are offered, whether an answer is taken — by running the session
//! and controller `ahma_tui` already has. The page is first-party files
//! embedded in the binary that draw what the server sends and send back what the
//! human did.

pub mod assets;
pub mod auth;
pub mod dto;
pub mod frontend;
pub mod guard;
pub mod headers;
pub mod listen;
pub mod policy;
pub mod project;
pub mod server;
pub mod tty;

use anyhow::{Context, Result};
use auth::Auth;
use frontend::WebFrontend;
use server::AppCtx;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// What `ahma gui` is started with.
pub struct GuiOptions {
    pub connect: Option<String>,
    pub profile: Option<String>,
    pub path: Option<std::path::PathBuf>,
    pub token_prefs: ahma_tui::TokenPrefs,
    /// Port on loopback; 0 picks a free one.
    pub port: u16,
    pub open_browser: bool,
}

/// Run the browser surface until it is stopped or the session ends.
///
/// The same session and controller as `ahma tui`, behind a loopback-only
/// listener that a tab pairs with using a code shown only on this terminal
/// (SPEC R-GUI).
pub async fn run_gui(options: GuiOptions) -> Result<()> {
    // Before anything is started: with no terminal there is nowhere safe to show
    // the pairing code.
    let terminal = tty::Terminal::open()?;

    let connection =
        ahma_tui::prepare_connection(options.connect.as_deref(), options.path.as_deref()).await?;

    let (listeners, port) = listen::bind(options.port).await?;
    let url = format!("http://127.0.0.1:{port}/");
    terminal.say(&tty::banner(&url));

    let first = Arc::new(AtomicBool::new(true));
    let announce: auth::Announce = {
        let terminal = terminal.clone();
        Arc::new(move |code: &str| {
            terminal.say(&tty::code_line(code, first.swap(false, Ordering::SeqCst)));
        })
    };
    let auth = Arc::new(Auth::new(announce));
    {
        let auth = auth.clone();
        terminal.on_enter(move || auth.unlock())?;
    }

    let (frontend, handles) = WebFrontend::new();
    let ctx = Arc::new(AppCtx {
        auth: auth.clone(),
        inputs: handles.inputs.clone(),
        published: handles.published.clone(),
    });
    let app = server::router(ctx);

    // The session lives on a thread of its own, on its own runtime: its state is
    // not `Send`, and the server talks to it over channels only.
    let (finished_tx, finished_rx) = tokio::sync::oneshot::channel::<Result<()>>();
    let workspace = options.path.clone();
    let profile = options.profile.clone();
    let token_prefs = options.token_prefs;
    let session_thread = std::thread::Builder::new()
        .name("ahma-gui-session".into())
        .spawn(move || {
            let result = (|| -> Result<()> {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .context("cannot start the session's runtime")?;
                runtime.block_on(async move {
                    let mut session = ahma_tui::session::Session::start(
                        &connection,
                        ahma_tui::session::SessionOptions {
                            profile_override: profile,
                            workspace_path: workspace,
                            token_prefs,
                            initial_command: None,
                            unicode: true,
                            surface: ahma_common::hub::Surface::Gui,
                            caps: policy::capabilities(),
                        },
                    );
                    let mut frontend = frontend;
                    ahma_tui::session::run_session(&mut session, &mut frontend).await
                })
            })();
            let _ = finished_tx.send(result);
        })
        .context("cannot start the session thread")?;

    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let server = tokio::spawn(listen::serve(listeners, app, stop_rx));

    if options.open_browser
        && let Err(e) = webbrowser::open(&url)
    {
        terminal.say(&format!(
            "Could not open a browser ({e}); open {url} yourself.\n"
        ));
    }

    let session_result = tokio::select! {
        _ = tokio::signal::ctrl_c() => None,
        finished = finished_rx => Some(finished.unwrap_or(Ok(()))),
    };

    // Stop serving, which drops the senders the session's frontend listens on,
    // which ends its loop, which drops its hub subscription.
    let _ = stop_tx.send(true);
    drop(handles);
    let _ = server.await;
    let _ = tokio::task::spawn_blocking(move || session_thread.join()).await;
    session_result.unwrap_or(Ok(()))
}
