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
//! `ahma desktop` runs the same server with the page in a window of its own
//! ([`desktop`]), and needs no pairing.

pub mod assets;
pub mod auth;
pub mod clipboard;
pub mod desktop;
pub mod dto;
pub mod frontend;
pub mod guard;
pub mod headers;
pub mod idle;
pub mod launch;
pub mod listen;
pub mod main_thread;
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

/// What `ahma desktop` is started with.
pub struct DesktopOptions {
    pub connect: Option<String>,
    pub profile: Option<String>,
    pub path: Option<std::path::PathBuf>,
    pub token_prefs: ahma_tui::TokenPrefs,
    /// Stay attached to the terminal rather than return to the prompt.
    pub foreground: bool,
    /// Run as the background window a launcher started (internal).
    pub ready_pipe: bool,
}

impl DesktopOptions {
    /// The `ahma web` a desktop falls back to where there is no window.
    fn as_web(&self) -> WebOptions {
        WebOptions {
            connect: self.connect.clone(),
            profile: self.profile.clone(),
            path: self.path.clone(),
            token_prefs: self.token_prefs,
            port: 0,
            open_browser: true,
            copy_code: true,
            foreground: self.foreground,
            idle_timeout: Some(std::time::Duration::from_secs(60)),
            ready_pipe: false,
        }
    }
}

/// Run `ahma desktop`: open the page in a window of its own and return to the
/// prompt, or (with `--foreground`) stay attached until the window closes.
/// Where the system has no web engine, say so and run `ahma web` instead.
pub async fn run_desktop(options: DesktopOptions) -> Result<()> {
    if let Some(reason) = desktop::unavailable_reason() {
        eprintln!("{reason}");
        return run_web(options.as_web()).await;
    }
    if options.ready_pipe || options.foreground {
        let web = WebOptions {
            ready_pipe: options.ready_pipe,
            open_browser: false,
            copy_code: false,
            idle_timeout: None,
            ..options.as_web()
        };
        return serve(web, Reporter::Window).await;
    }
    ahma_tui::prepare_connection(options.connect.as_deref(), options.path.as_deref()).await?;
    let ready = launch::start_server().await?;
    if let Some(reason) = &ready.unavailable {
        eprintln!("ahma desktop could not open a window: {reason}. Opening ahma web instead.");
        return run_launcher(options.as_web()).await;
    }
    eprintln!(
        "ahma desktop opened a window for {}. It stops when you close the window.",
        workspace_label(options.path.as_deref())
    );
    Ok(())
}

/// The directory a session is for, as a window title or a message shows it.
fn workspace_label(path: Option<&std::path::Path>) -> String {
    path.map(std::path::Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| ".".into())
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
    /// Nowhere: the one client is this process's own window (`ahma desktop`).
    Window,
}

/// Run the browser surface until it is stopped, the session ends, or no tab
/// has been connected for the idle timeout.
///
/// The same session and controller as `ahma tui`, behind a loopback-only
/// listener that a tab pairs with using a code only the human who started it
/// was shown (SPEC R-GUI).
struct PairingSetup {
    auth: Arc<Auth>,
    pairing: Pairing,
    first_code: Arc<OnceLock<String>>,
    clear_code: Option<Arc<dyn Fn() + Send + Sync>>,
}

fn build_pairing(reporter: &Reporter, url: &str, copy_code: bool) -> Result<PairingSetup> {
    let pairing = match reporter {
        Reporter::Terminal(_) => Pairing::Terminal,
        Reporter::ReadyPipe => Pairing::Background { copied: copy_code },
        Reporter::Window => Pairing::Linked,
    };
    // The first code, which a launcher may have put on the clipboard.
    let first_code: Arc<OnceLock<String>> = Arc::new(OnceLock::new());
    let announce: auth::Announce = match reporter {
        Reporter::Terminal(terminal) => {
            terminal.say(&tty::banner(url));
            let terminal = terminal.clone();
            let first = Arc::new(AtomicBool::new(true));
            Arc::new(move |code: &str| {
                terminal.say(&tty::code_line(code, first.swap(false, Ordering::SeqCst)));
            })
        }
        Reporter::ReadyPipe => {
            let url = url.to_string();
            let first_code = first_code.clone();
            Arc::new(move |code: &str| {
                // Only the first: the launcher reads one line and is gone, and
                // standard output is never written again (SPEC R5.6.1).
                if first_code.set(code.to_string()).is_ok() {
                    let line = launch::ready_line(&launch::Ready {
                        url: url.clone(),
                        code: code.to_string(),
                        unavailable: None,
                    });
                    write_ready_line(&line);
                }
            })
        }
        // No code is ever shown: pairing is closed.
        Reporter::Window => Arc::new(|_: &str| {}),
    };
    let auth = Arc::new(Auth::new(announce).with_policy(PairingPolicy {
        burn_on_reuse: matches!(pairing, Pairing::Background { copied: true }),
        pairing_closed: pairing == Pairing::Linked,
        ..PairingPolicy::default()
    }));
    if let Reporter::Terminal(terminal) = reporter {
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
    Ok(PairingSetup {
        auth,
        pairing,
        first_code,
        clear_code,
    })
}

fn spawn_session_thread(
    connection: ahma_tui::ResolvedConnection,
    options: &WebOptions,
    pairing: Pairing,
    frontend: WebFrontend,
) -> Result<(
    std::thread::JoinHandle<()>,
    tokio::sync::oneshot::Receiver<Result<()>>,
)> {
    // The session lives on a thread of its own, on its own runtime: its state is
    // not `Send`, and the server talks to it over channels only.
    let (finished_tx, finished_rx) = tokio::sync::oneshot::channel::<Result<()>>();
    let workspace = options.path.clone();
    let profile = options.profile.clone();
    let token_prefs = options.token_prefs;
    let surface = if pairing == Pairing::Linked {
        ahma_common::hub::Surface::Desktop
    } else {
        ahma_common::hub::Surface::Web
    };
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
                            surface,
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
    Ok((session_thread, finished_rx))
}

async fn open_desktop_window(
    url: &str,
    auth: &Auth,
    options: &WebOptions,
    stop: &tokio::sync::watch::Sender<bool>,
) -> Option<OpenWindow> {
    let (_tab, token) = auth.mint_tab();
    let (window, unavailable) = match open_window(url, token, options.path.as_deref()).await {
        Ok(window) => (Some(window), None),
        Err(reason) => {
            stop.send_replace(true);
            tracing::warn!("desktop: {reason}");
            (None, Some(reason))
        }
    };
    if options.ready_pipe {
        write_ready_line(&launch::ready_line(&launch::Ready {
            url: url.to_string(),
            code: String::new(),
            unavailable,
        }));
    }
    window
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

    let PairingSetup {
        auth,
        pairing,
        first_code,
        clear_code,
    } = build_pairing(&reporter, &url, options.copy_code)?;

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

    let (session_thread, finished_rx) =
        spawn_session_thread(connection, &options, pairing, frontend)?;

    let server = tokio::spawn(listen::serve(listeners, app, stop.subscribe()));
    if let Some(timeout) = options.idle_timeout {
        tokio::spawn(idle::stop_when_idle(idle, timeout, stop.clone()));
    }

    // `ahma desktop`: the window, on the main thread, with the one token.
    let window = if pairing == Pairing::Linked {
        open_desktop_window(&url, &auth, &options, &stop).await
    } else {
        None
    };
    let (closer, window_closed) = match window {
        Some(w) => (w.closer, Some(w.closed)),
        None => (None, None),
    };
    let window_closed = async move {
        match window_closed {
            Some(closed) => {
                let _ = closed.await;
            }
            None => std::future::pending::<()>().await,
        }
    };

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
        _ = window_closed => None,
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
    close_window(closer);
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

/// Write the one line a launcher reads. Standard output is never written
/// again (SPEC R5.6.1).
fn write_ready_line(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(line.as_bytes());
    let _ = out.flush();
}

/// An open `ahma desktop` window: how to close it, and when it closed.
struct OpenWindow {
    closer: Option<WindowCloser>,
    closed: tokio::sync::oneshot::Receiver<()>,
}

#[cfg(any(target_os = "macos", windows))]
type WindowCloser = desktop::window::Closer;
#[cfg(not(any(target_os = "macos", windows)))]
type WindowCloser = ();

/// Open the window on the main thread and wait until it has opened (or
/// could not).
#[cfg(any(target_os = "macos", windows))]
async fn open_window(
    url: &str,
    token: String,
    path: Option<&std::path::Path>,
) -> std::result::Result<OpenWindow, String> {
    let (opened_tx, opened_rx) = std::sync::mpsc::channel();
    let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
    let title = format!("ahma — {}", workspace_label(path));
    let url = url.to_string();
    main_thread::run(Box::new(move || {
        desktop::window::run(title, url, token, opened_tx);
        let _ = closed_tx.send(());
    }))
    .map_err(|e| e.to_string())?;
    let opened = tokio::task::spawn_blocking(move || opened_rx.recv())
        .await
        .map_err(|e| e.to_string())?
        .map_err(|_| "the window closed before it opened".to_string())??;
    Ok(OpenWindow {
        closer: Some(opened),
        closed: closed_rx,
    })
}

#[cfg(not(any(target_os = "macos", windows)))]
async fn open_window(
    _url: &str,
    _token: String,
    _path: Option<&std::path::Path>,
) -> std::result::Result<OpenWindow, String> {
    Err(desktop::unavailable_reason()
        .unwrap_or("no window on this system")
        .to_string())
}

/// Ask the window to close, if there is one.
#[cfg(any(target_os = "macos", windows))]
fn close_window(closer: Option<WindowCloser>) {
    if let Some(closer) = closer {
        let _ = closer.send_event(desktop::window::Request::Close);
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
fn close_window(_closer: Option<WindowCloser>) {}

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
