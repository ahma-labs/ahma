//! The page: first-party files embedded in the binary (SPEC R-GUI.10).
//!
//! There is no build step at `cargo build` time and no file read at run time:
//! what is served is exactly what was compiled in, and what was compiled in is
//! exactly what is committed under `web/`. A request names a path in this table
//! or gets a 404; nothing is looked up on disk, so no path can be made to reach
//! anything else.

/// One embedded file.
pub struct Asset {
    pub path: &'static str,
    pub content_type: &'static str,
    pub body: &'static str,
}

const HTML: &str = "text/html; charset=utf-8";
const JS: &str = "text/javascript; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";
const SVG: &str = "image/svg+xml";

/// Every file that is served. `dist/` is the compiler's output, committed; a test
/// fails if a file appears there that is not listed here.
pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/",
        content_type: HTML,
        body: include_str!("../web/index.html"),
    },
    Asset {
        path: "/app.css",
        content_type: CSS,
        body: include_str!("../web/app.css"),
    },
    Asset {
        path: "/favicon.svg",
        content_type: SVG,
        body: include_str!("../web/favicon.svg"),
    },
    Asset {
        path: "/dist/act.js",
        content_type: JS,
        body: include_str!("../web/dist/act.js"),
    },
    Asset {
        path: "/dist/answer.js",
        content_type: JS,
        body: include_str!("../web/dist/answer.js"),
    },
    Asset {
        path: "/dist/api.js",
        content_type: JS,
        body: include_str!("../web/dist/api.js"),
    },
    Asset {
        path: "/dist/dom.js",
        content_type: JS,
        body: include_str!("../web/dist/dom.js"),
    },
    Asset {
        path: "/dist/format.js",
        content_type: JS,
        body: include_str!("../web/dist/format.js"),
    },
    Asset {
        path: "/dist/main.js",
        content_type: JS,
        body: include_str!("../web/dist/main.js"),
    },
    Asset {
        path: "/dist/md.js",
        content_type: JS,
        body: include_str!("../web/dist/md.js"),
    },
    Asset {
        path: "/dist/sse.js",
        content_type: JS,
        body: include_str!("../web/dist/sse.js"),
    },
    Asset {
        path: "/dist/store.js",
        content_type: JS,
        body: include_str!("../web/dist/store.js"),
    },
    Asset {
        path: "/dist/stream.js",
        content_type: JS,
        body: include_str!("../web/dist/stream.js"),
    },
    Asset {
        path: "/dist/view/chat.js",
        content_type: JS,
        body: include_str!("../web/dist/view/chat.js"),
    },
    Asset {
        path: "/dist/view/gate.js",
        content_type: JS,
        body: include_str!("../web/dist/view/gate.js"),
    },
    Asset {
        path: "/dist/view/header.js",
        content_type: JS,
        body: include_str!("../web/dist/view/header.js"),
    },
    Asset {
        path: "/dist/view/log.js",
        content_type: JS,
        body: include_str!("../web/dist/view/log.js"),
    },
    Asset {
        path: "/dist/view/pair.js",
        content_type: JS,
        body: include_str!("../web/dist/view/pair.js"),
    },
    Asset {
        path: "/dist/view/toast.js",
        content_type: JS,
        body: include_str!("../web/dist/view/toast.js"),
    },
    Asset {
        path: "/dist/view/work.js",
        content_type: JS,
        body: include_str!("../web/dist/view/work.js"),
    },
    Asset {
        path: "/dist/wire.js",
        content_type: JS,
        body: include_str!("../web/dist/wire.js"),
    },
];

/// The asset served at `path`, if there is one.
pub fn lookup(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|a| a.path == path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A compiled file that is not in the table is a file the page imports and
    /// cannot load: the failure should be here, not in a browser.
    #[test]
    fn every_compiled_file_is_served() {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).expect("web/dist exists") {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, out);
                } else {
                    out.push(path);
                }
            }
        }
        let web = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("web");
        let mut found = Vec::new();
        walk(&web.join("dist"), &mut found);
        assert!(found.len() > 5, "the compiled page is there: {found:?}");
        for path in found {
            if path.extension().is_some_and(|e| e == "js") {
                let url = format!(
                    "/{}",
                    path.strip_prefix(&web)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/")
                );
                assert!(
                    lookup(&url).is_some(),
                    "{url} is compiled but not embedded: add it to ASSETS"
                );
            }
        }
        // And the other way: nothing is listed that is empty.
        for asset in ASSETS {
            assert!(!asset.body.is_empty(), "{} is empty", asset.path);
        }
    }

    #[test]
    fn a_path_not_in_the_table_is_nothing() {
        for path in [
            "/etc/passwd",
            "/../Cargo.toml",
            "/dist/../Cargo.toml",
            "/dist/",
            "/index.html",
            "",
        ] {
            assert!(lookup(path).is_none(), "{path:?}");
        }
    }

    /// The page asks for nothing the policy would refuse: no inline script or
    /// style, no remote source.
    #[test]
    fn the_shell_is_self_contained() {
        let html = lookup("/").unwrap().body;
        assert!(!html.contains("<style"), "no inline style");
        assert!(!html.contains(" style="), "no style attributes");
        assert!(!html.contains("onclick") && !html.contains("javascript:"));
        for tag in html.split('<').skip(1) {
            if tag.starts_with("script") {
                assert!(
                    tag.contains("src=\"/dist/"),
                    "only module scripts from /dist: {tag}"
                );
            }
            for attr in ["src=\"http", "href=\"http", "src=\"//", "href=\"//"] {
                assert!(!tag.contains(attr), "no remote reference: {tag}");
            }
        }
    }
}
