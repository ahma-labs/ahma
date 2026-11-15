//! `ahma web`: the browser as a second surface over the same session as the
//! terminal (SPEC R-GUI).
//!
//! The server owns everything that decides anything — what a human is shown,
//! what they are offered, whether an answer is taken — by running the session
//! and controller `ahma_tui` already has. The page is first-party files
//! embedded in the binary that draw what the server sends and send back what the
//! human did.
//!
//! A plain `ahma web` only launches ([`launch`]): the server it starts runs in
//! the background and stops by itself when no tab is left ([`idle`]).

pub mod assets;
pub mod auth;
pub mod clipboard;
pub mod dto;
pub mod frontend;
pub mod guard;
pub mod headers;
pub mod idle;
pub mod launch;
pub mod listen;
pub mod policy;
pub mod project;
pub mod server;
pub mod tty;

use anyhow::{Context, Result};
use auth::{Auth, PairingPolicy};
use frontend::WebFrontend;
use server::{AppCtx, Pairing};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

/// How long stopping waits for the server, then the session, before giving up
/// on a clean exit.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);
/// How long a closing session waits to end its hub MCP session.
const SESSION_CLOSE_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

/// What `ahma web` is started with.
pub struct WebOptions {
    pub connect: Option<String>,
    pub profile: Option<String>,
    pub path: Option<std::path::PathBuf>,
    pub token_prefs: ahma_tui::TokenPrefs,
    /// Port on loopback; 0 picks a free one.
    pub port: u16,
    pub open_browser: bool,
    /// Copy the pairing code to the clipboard.
    pub copy_code: bool,
    /// Stay attached to the terminal rather than return to the prompt.
    pub foreground: bool,
    /// Stop once no tab has been connected for this long; `None` never stops.
    pub idle_timeout: Option<std::time::Duration>,
    /// Run as the background server a launcher started (internal).
    pub ready_pipe: bool,
}

/// Run `ahma web`: launch a background server and return, or (with
/// `--foreground`) run one attached to this terminal.
pub async fn run_web(options: WebOptions) -> Result<()> {
    if options.ready_pipe {
        return serve(options, Reporter::ReadyPipe).await;
    }
    if options.foreground {
        // Before anything is started: with no terminal there is nowhere safe to
        // show the pairing code.
        let terminal = tty::Terminal::open()?;
        return serve(options, Reporter::Terminal(terminal)).await;
    }
    run_launcher(options).await
}

/// The launcher: check the scope and the hub where the warnings can be read,
/// start the background server, hand the human its address and code, return.
async fn run_launcher(options: WebOptions) -> Result<()> {
    // The code is shown on the controlling terminal, never standard output
    // (R-GUI.2): with neither a terminal nor the clipboard there is nowhere to
    // put it.
    let terminal = match tty::Terminal::open() {
        Ok(t) => Some(t),
        Err(e) if !options.copy_code => return Err(e),
        Err(_) => None,
    };
    // Scope warnings, and the hub started if it is not running: here, while
    // the human is still looking.
    ahma_tui::prepare_connection(options.connect.as_deref(), options.path.as_deref()).await?;

    let ready = launch::start_server().await?;
    let copied = if options.copy_code {
        let code = ready.code.clone();
        tokio::task::spawn_blocking(move || clipboard::copy(&code))
            .await
            .ok()
            .flatten()
    } else {
        None
    };
    if copied.is_none() && terminal.is_none() {
        anyhow::bail!(
            "ahma web could not copy the pairing code to the clipboard, and there is no \
             terminal to show it on. Run it from a terminal."
        );
    }
    let opened = if options.open_browser {
        webbrowser::open(&ready.url).map_err(|e| e.to_string())
    } else {
        Ok(())
    };
    let summary = launch::summary(&ready, copied, opened);
    match &terminal {
        Some(t) => t.say(&summary),
        // The code is on the clipboard only: say everything else.
        None => eprint!("{}", summary.replace(&ready.code, "(on your clipboard)")),
    }
    Ok(())
}

/// Where a server shows its pairing codes.
enum Reporter {
    /// The controlling terminal, attached for the server's life.
    Terminal(Arc<tty::Terminal>),
    /// Its launcher, once, on standard output.
    ReadyPipe,
}

/// Run the browser surface until it is stopped, the session ends, or no tab
/// has been connected for the idle timeout.
///
/// The same session and controller as `ahma tui`, behind a loopback-only
/// listener that a tab pairs with using a code only the human who started it
/// was shown (SPEC R-GUI).
async fn serve(options: WebOptions, reporter: Reporter) -> Result<()> {
    let connection =
        ahma_tui::prepare_connection(options.connect.as_deref(), options.path.as_deref()).await?;

    let (listeners, port) = listen::bind(options.port).await?;
    let url = format!("http://127.0.0.1:{port}/");

    let pairing = match reporter {
        Reporter::Terminal(_) => Pairing::Terminal,
        Reporter::ReadyPipe => Pairing::Background {
            copied: options.copy_code,
        },
    };
    // The first code, which a launcher may have put on the clipboard.
    let first_code: Arc<OnceLock<String>> = Arc::new(OnceLock::new());
    let announce: auth::Announce = match &reporter {
        Reporter::Terminal(terminal) => {
            terminal.say(&tty::banner(&url));
            let terminal = terminal.clone();
            let first = Arc::new(AtomicBool::new(true));
            Arc::new(move |code: &str| {
                terminal.say(&tty::code_line(code, first.swap(false, Ordering::SeqCst)));
            })
        }
        Reporter::ReadyPipe => {
            let url = url.clone();
            let first_code = first_code.clone();
            Arc::new(move |code: &str| {
                // Only the first: the launcher reads one line and is gone, and
                // standard output is never written again (SPEC R5.6.1).
                if first_code.set(code.to_string()).is_ok() {
                    let line = launch::ready_line(&launch::Ready {
                        url: url.clone(),
                        code: code.to_string(),
                    });
                    let mut out = std::io::stdout().lock();
                    let _ = out.write_all(line.as_bytes());
                    let _ = out.flush();
                }
            })
        }
    };
    let auth = Arc::new(Auth::new(announce).with_policy(PairingPolicy {
        burn_on_reuse: matches!(pairing, Pairing::Background { copied: true }),
        ..PairingPolicy::default()
    }));
    if let Reporter::Terminal(terminal) = &reporter {
        let auth = auth.clone();
        terminal.on_enter(move || auth.unlock())?;
    }
    // Once a tab has paired with the code it is spent: take it off the
    // clipboard, unless the human has copied something else since.
    let clear_code: Option<Arc<dyn Fn() + Send + Sync>> = match pairing {
        Pairing::Background { copied: true } => {
            let first_code = first_code.clone();
            Some(Arc::new(move || {
                if let Some(code) = first_code.get().cloned() {
                    std::thread::spawn(move || clipboard::clear_if_holds(&code));
                }
            }))
        }
        _ => None,
    };

    let (frontend, handles) = WebFrontend::new();
    let stop = Arc::new(tokio::sync::watch::channel(false).0);
    let idle = Arc::new(idle::Idle::new(std::time::Instant::now()));
    let ctx = Arc::new(AppCtx {
        auth: auth.clone(),
        inputs: handles.inputs.clone(),
        published: handles.published.clone(),
        stop: stop.clone(),
        idle: idle.clone(),
        pairing,
        on_paired: clear_code.clone(),
    });
    let app = server::router(ctx);

    // The session lives on a thread of its own, on its own runtime: its state is
    // not `Send`, and the server talks to it over channels only.
    let (finished_tx, finished_rx) = tokio::sync::oneshot::channel::<Result<()>>();
    let workspace = options.path.clone();
    let profile = options.profile.clone();
    let token_prefs = options.token_prefs;
    let session_thread = std::thread::Builder::new()
        .name("ahma-web-session".into())
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
                            surface: ahma_common::hub::Surface::Web,
                            caps: policy::capabilities(),
                        },
                    );
                    let mut frontend = frontend;
                    let result = ahma_tui::session::run_session(&mut session, &mut frontend).await;
                    // End the hub MCP session before this runtime goes, which
                    // would cancel the task that does it (and leave the hub's
                    // worker for it running until it idles out).
                    session.close(SESSION_CLOSE_GRACE).await;
                    result
                })
            })();
            let _ = finished_tx.send(result);
        })
        .context("cannot start the session thread")?;

    let server = tokio::spawn(listen::serve(listeners, app, stop.subscribe()));
    if let Some(timeout) = options.idle_timeout {
        tokio::spawn(idle::stop_when_idle(idle, timeout, stop.clone()));
    }

    if let Reporter::Terminal(terminal) = &reporter
        && options.open_browser
        && let Err(e) = webbrowser::open(&url)
    {
        terminal.say(&format!(
            "Could not open a browser ({e}); open {url} yourself.\n"
        ));
    }

    let mut stopped = stop.subscribe();
    let session_result = tokio::select! {
        _ = tokio::signal::ctrl_c() => None,
        _ = terminated() => None,
        _ = stopped.wait_for(|stop| *stop) => None,
        finished = finished_rx => Some(finished.unwrap_or(Ok(()))),
    };
    // A second Ctrl-C while shutting down means now.
    tokio::spawn(async {
        if tokio::signal::ctrl_c().await.is_ok() {
            std::process::exit(130);
        }
    });

    // Stop serving: every open event stream says `bye` and ends, the server
    // drops the senders the session's frontend listens on, which ends its loop,
    // which closes its hub MCP session and drops its hub subscription.
    stop.send_replace(true);
    drop(handles);
    if tokio::time::timeout(SHUTDOWN_GRACE, server).await.is_err() {
        tracing::warn!("web: the server did not stop within {SHUTDOWN_GRACE:?}");
    }
    let joined = tokio::task::spawn_blocking(move || session_thread.join());
    if tokio::time::timeout(SHUTDOWN_GRACE, joined).await.is_err() {
        tracing::warn!("web: the session did not end within {SHUTDOWN_GRACE:?}");
    }
    // A code nobody used is still on the clipboard: take it off.
    if let Some(code) = first_code.get().cloned()
        && clear_code.is_some()
    {
        let _ = tokio::task::spawn_blocking(move || clipboard::clear_if_holds(&code)).await;
    }
    session_result.unwrap_or(Ok(()))
}

/// Resolves when the process is asked to terminate (`kill`, a logout), so a
/// background server still closes its session and clears the clipboard.
async fn terminated() {
    #[cfg(unix)]
    {
        if let Ok(mut term) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            term.recv().await;
            return;
        }
    }
    std::future::pending::<()>().await
}
