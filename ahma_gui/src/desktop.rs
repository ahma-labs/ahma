//! `ahma desktop`: the page in a window of ahma's own, on the operating
//! system's web engine (SPEC R-DESK).
//!
//! The window and the server are one process, so there is nothing to pair:
//! the server mints the one tab token before the window opens, the window
//! hands it to the page through a script the engine injects, and pairing is
//! closed ([`crate::auth::PairingPolicy::pairing_closed`]). The token never
//! leaves the process's memory except into that page.
//!
//! The engine is the system's own (WKWebView on macOS, WebView2 on Windows),
//! so the window adds no engine to the binary. Linux has no system engine
//! (WebKitGTK is a shared library headless machines lack), so there
//! `ahma desktop` explains and runs `ahma web` instead.

/// The script the engine runs before the page's own code: it gives the page
/// its token, on this server's origin only, as a property nothing can change.
pub fn init_script(origin: &str, token: &str) -> String {
    // JSON string literals: nothing in either can end the string early.
    let origin = serde_json::to_string(origin).unwrap_or_default();
    let token = serde_json::to_string(token).unwrap_or_default();
    format!(
        "if (location.origin === {origin}) {{ Object.defineProperty(globalThis, \"__ahmaDesktop\", \
         {{ value: Object.freeze({{ token: {token} }}), writable: false, configurable: false, \
         enumerable: false }}); }}"
    )
}

/// The origin of `url`: scheme, host and port, without a trailing slash, as
/// `location.origin` reads.
pub fn origin_of(url: &str) -> String {
    let after_scheme = url.find("://").map_or(0, |i| i + 3);
    let end = url[after_scheme..]
        .find('/')
        .map_or(url.len(), |i| after_scheme + i);
    url[..end].to_string()
}

/// Whether the window may navigate to `url`: only within its own server.
/// Anything else (a link in chat text is inert, but a page could still try)
/// opens in the default browser instead, where it has none of this window's
/// authority.
pub fn may_navigate(url: &str, origin: &str) -> bool {
    url == origin
        || url.strip_prefix(origin).is_some_and(|rest| {
            rest.starts_with('/') || rest.starts_with('?') || rest.starts_with('#')
        })
}

/// Why there is no window on this build, if there is none.
pub fn unavailable_reason() -> Option<&'static str> {
    if cfg!(any(target_os = "macos", windows)) {
        None
    } else {
        Some(
            "ahma desktop needs a web engine that is part of the operating system (WKWebView on \
             macOS, WebView2 on Windows). This system has none, so ahma web opens the same page \
             in your browser instead.",
        )
    }
}

#[cfg(any(target_os = "macos", windows))]
pub mod window {
    //! The window itself, on the process's main thread.

    use tao::event::{Event, WindowEvent};
    use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
    use tao::platform::run_return::EventLoopExtRunReturn;
    use tao::window::WindowBuilder;
    use wry::{NewWindowResponse, WebViewBuilder};

    /// Something the server asks of the window.
    #[derive(Debug, Clone, Copy)]
    pub enum Request {
        /// The server has stopped: close.
        Close,
    }

    /// A handle the server keeps to close the window.
    pub type Closer = EventLoopProxy<Request>;

    /// Open the window on `url` with `token`, report whether it opened on
    /// `opened`, and run it until it is closed. Must run on the main thread.
    pub fn run(
        title: String,
        url: String,
        token: String,
        opened: std::sync::mpsc::Sender<Result<Closer, String>>,
    ) {
        let mut event_loop = EventLoopBuilder::<Request>::with_user_event().build();
        #[cfg(target_os = "macos")]
        {
            // A plain binary, not an app bundle: say it is an app with a
            // window, so it is in the Dock and in Cmd-Tab.
            use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
            event_loop.set_activation_policy(ActivationPolicy::Regular);
        }
        let window = match WindowBuilder::new()
            .with_title(title)
            .with_inner_size(tao::dpi::LogicalSize::new(1280.0, 860.0))
            .build(&event_loop)
        {
            Ok(w) => w,
            Err(e) => {
                let _ = opened.send(Err(format!("cannot open a window: {e}")));
                return;
            }
        };
        let origin = super::origin_of(&url);
        let nav_origin = origin.clone();
        let webview = WebViewBuilder::new()
            .with_url(&url)
            .with_initialization_script_for_main_only(super::init_script(&origin, &token), true)
            .with_navigation_handler(move |to| {
                if super::may_navigate(&to, &nav_origin) {
                    true
                } else {
                    let _ = webbrowser::open(&to);
                    false
                }
            })
            .with_new_window_req_handler(|to, _features| {
                let _ = webbrowser::open(&to);
                NewWindowResponse::Deny
            })
            .with_devtools(false)
            .build(&window);
        let _webview = match webview {
            Ok(w) => w,
            Err(e) => {
                let _ = opened.send(Err(format!("the system web engine is not available ({e})")));
                return;
            }
        };
        let _ = opened.send(Ok(event_loop.create_proxy()));
        event_loop.run_return(move |event, _, control_flow| {
            *control_flow = ControlFlow::Wait;
            match event {
                Event::WindowEvent {
                    event: WindowEvent::CloseRequested,
                    ..
                }
                | Event::UserEvent(Request::Close) => *control_flow = ControlFlow::Exit,
                _ => {}
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The token reaches the page on the server's own origin only, quoted so
    /// nothing in it can become script, as a property that cannot be changed.
    #[test]
    fn the_init_script_gives_the_token_on_this_origin_only() {
        let script = init_script("http://127.0.0.1:5000", "1.abc\"};alert(1)//");
        assert!(script.starts_with("if (location.origin === \"http://127.0.0.1:5000\")"));
        assert!(script.contains(r#""1.abc\"};alert(1)//""#), "{script}");
        assert!(script.contains("writable: false") && script.contains("configurable: false"));
        assert!(script.contains("Object.freeze"));
    }

    #[test]
    fn the_origin_is_scheme_host_and_port() {
        assert_eq!(origin_of("http://127.0.0.1:5000/"), "http://127.0.0.1:5000");
        assert_eq!(
            origin_of("http://127.0.0.1:5000/x/y?z"),
            "http://127.0.0.1:5000"
        );
        assert_eq!(origin_of("http://127.0.0.1:5000"), "http://127.0.0.1:5000");
    }

    /// The window stays on its own server: another port on the same host, a
    /// look-alike host, or anywhere else goes to the default browser.
    #[test]
    fn the_window_navigates_only_within_its_own_server() {
        let origin = "http://127.0.0.1:5000";
        for ok in [
            "http://127.0.0.1:5000",
            "http://127.0.0.1:5000/",
            "http://127.0.0.1:5000/dist/main.js",
            "http://127.0.0.1:5000/?x",
            "http://127.0.0.1:5000#top",
        ] {
            assert!(may_navigate(ok, origin), "{ok}");
        }
        for bad in [
            "http://127.0.0.1:50001/",
            "http://127.0.0.1:5000.evil.example/",
            "http://127.0.0.1:500/",
            "https://example.com/",
            "file:///etc/passwd",
            "javascript:alert(1)",
        ] {
            assert!(!may_navigate(bad, origin), "{bad}");
        }
    }

    #[test]
    fn only_systems_with_an_engine_have_a_window() {
        assert_eq!(
            unavailable_reason().is_none(),
            cfg!(any(target_os = "macos", windows))
        );
    }
}
