//! Web links a sandboxed command asks ahma to open (SPEC R-LINK).
//!
//! Pure policy, no I/O: what counts
//! as a web link (R-LINK.2), which hosts are local (R-LINK.4), whether a grant
//! covers a link and what to ask when none does (R-LINK.4, R-LINK.5), the one
//! command line that *is* a link open (R-LINK.10), and the words a human reads
//! when deciding (R-LINK.7). The broker (`links::broker`) does the I/O: it
//! resolves the host, reads the grants, asks, and opens.
//!
//! Site matching is [`HostPattern`], the one host matcher, so `[open_url]
//! allow` and `[open_url] repos` read exactly like `[network] allow` (exact,
//! `*.one-label`, `*`). The settings, the guidance and where a saved grant
//! applies ([`OpenUrlBinding`]) live in [`ahma_common::open_url`].

use crate::egress::HostPattern;
use ahma_common::open_url::{GUIDANCE, OpenUrlBinding, OpenUrlSettings};
use std::net::Ipv4Addr;
use std::path::Path;

/// The longest address ahma opens, raw or as opened (SPEC R-LINK.2). Real
/// links (a PR, an OAuth device page, a docs page) are far shorter; a longer
/// one is a payload, not a link.
pub const MAX_URL_BYTES: usize = 8 * 1024;

/// How many links one command may open (SPEC R-LINK.8). Each is a tab the
/// human sees; a command opening more is sending data, not showing a page.
pub const MAX_LINKS_PER_COMMAND: usize = 5;

/// How much of an address a question shows before it says how much is left.
const SHOWN_ADDRESS_CHARS: usize = 300;

/// Said when the link is to this machine or the local network.
pub const LOCAL_NOTE: &str = "This address is on your own machine or local network, where a \
     link can make a local service act for you. \"Always (any site)\" never covers it.";

/// Said under `--restrict-network` for a host outside the egress allow list.
pub fn outside_egress_note(host: &str) -> String {
    format!(
        "{host} is not on your network allow list. Allowing opens its links in your browser \
         only; commands still cannot reach it."
    )
}

/// Said for an internationalised name (an `xn--` label).
pub const IDN_NOTE: &str = "This name is internationalised (`xn--`) and can look like another \
     site's name when written in its own script. Check it before you allow it.";

/// A web link that passed every check, in the form ahma opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkTarget {
    url: url::Url,
    host: String,
    class: HostClass,
}

/// Where a host is, as far as its name or address says (SPEC R-LINK.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostClass {
    /// On the public internet.
    Public,
    /// This machine or the local network: loopback, private, link-local,
    /// shared (CGNAT) and unspecified IPv4; `localhost`, `*.localhost`,
    /// `.local`, `.internal`, `.lan`, `.home.arpa`, and a name with no dot.
    Local,
}

/// Why an argument is not a link ahma opens. Never asked about: no answer
/// would make it one (SPEC R-LINK.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotALink {
    Empty,
    /// Longer than [`MAX_URL_BYTES`], raw or as it would be opened.
    TooLong(usize),
    /// Not `http://` or `https://`: a file path, `file:`, a custom scheme,
    /// `javascript:`, `data:`, `mailto:`.
    NotWeb,
    /// Whitespace, a control character or a backslash, which parsers read
    /// differently (WHATWG drops tabs and newlines and reads `\` as `/`).
    Unsafe(char),
    Malformed(String),
    /// `user:pass@` before the host: `https://github.com@evil.example/`
    /// names `evil.example`.
    UserInfo,
    NoHost,
    /// `http://[::1]/`: not opened in this version; use a name.
    Ipv6Literal,
}

impl NotALink {
    /// What it is, for "`<arg>` is `<this>`, so it was not opened".
    pub fn what(&self) -> String {
        match self {
            NotALink::Empty => "empty".into(),
            NotALink::TooLong(n) => format!("{n} bytes long (the limit is {MAX_URL_BYTES})"),
            NotALink::NotWeb => "not an http or https link (a file, an app or another kind of \
                                 address)"
                .into(),
            NotALink::Unsafe(c) => format!("written with {c:?}, which links never need"),
            NotALink::Malformed(e) => format!("not a valid address ({e})"),
            NotALink::UserInfo => "an address with a user name or password before its host".into(),
            NotALink::NoHost => "an address without a host".into(),
            NotALink::Ipv6Literal => "an IPv6 address; use a host name".into(),
        }
    }
}

impl LinkTarget {
    /// Check `raw` as SPEC R-LINK.2 says, the raw text first and then the
    /// parsed address, so what is checked is what is opened.
    pub fn parse(raw: &str) -> Result<Self, NotALink> {
        if raw.is_empty() {
            return Err(NotALink::Empty);
        }
        if raw.len() > MAX_URL_BYTES {
            return Err(NotALink::TooLong(raw.len()));
        }
        let head = raw.get(..8).unwrap_or(raw).to_ascii_lowercase();
        if !(head.starts_with("http://") || head.starts_with("https://")) {
            return Err(NotALink::NotWeb);
        }
        if let Some(c) = raw
            .chars()
            .find(|c| c.is_whitespace() || c.is_control() || *c == '\\')
        {
            return Err(NotALink::Unsafe(c));
        }
        let url = url::Url::parse(raw).map_err(|e| NotALink::Malformed(e.to_string()))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(NotALink::NotWeb);
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(NotALink::UserInfo);
        }
        let (host, class) = match url.host() {
            Some(url::Host::Domain(d)) if !d.is_empty() => {
                let d = d.to_ascii_lowercase();
                let class = if domain_is_local(&d) {
                    HostClass::Local
                } else {
                    HostClass::Public
                };
                (d, class)
            }
            Some(url::Host::Ipv4(ip)) => (
                ip.to_string(),
                if ipv4_is_local(ip) {
                    HostClass::Local
                } else {
                    HostClass::Public
                },
            ),
            Some(url::Host::Ipv6(_)) => return Err(NotALink::Ipv6Literal),
            _ => return Err(NotALink::NoHost),
        };
        // Percent-encoding can lengthen it; the limit is on what is opened.
        if url.as_str().len() > MAX_URL_BYTES {
            return Err(NotALink::TooLong(url.as_str().len()));
        }
        Ok(Self { url, host, class })
    }

    /// The address as opened: the parser's serialization (ASCII, punycode
    /// host, percent-encoded), never the raw argument.
    pub fn as_str(&self) -> &str {
        self.url.as_str()
    }

    /// The host, lowercased ASCII (an internationalised name in punycode).
    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn class(&self) -> HostClass {
        self.class
    }

    /// Whether any label is punycode.
    pub fn is_internationalised(&self) -> bool {
        self.host.split('.').any(|l| l.starts_with("xn--"))
    }

    /// The address as a question shows it: whole up to
    /// `SHOWN_ADDRESS_CHARS`, else cut, saying how much was left out.
    pub fn shown(&self) -> String {
        let s = self.as_str();
        let total = s.chars().count();
        if total <= SHOWN_ADDRESS_CHARS {
            return s.to_string();
        }
        let head: String = s.chars().take(SHOWN_ADDRESS_CHARS).collect();
        format!("{head}… ({} more characters)", total - SHOWN_ADDRESS_CHARS)
    }

    /// What the execution audit log keeps (SPEC R-LINK.9): scheme, host,
    /// port and path, and only the length of the query and fragment, which
    /// is where tokens live.
    pub fn audit_form(&self) -> String {
        let mut base = self.url.clone();
        let query = base.query().map_or(0, str::len);
        let fragment = base.fragment().map_or(0, str::len);
        base.set_query(None);
        base.set_fragment(None);
        match (query, fragment) {
            (0, 0) => base.to_string(),
            _ => format!("{base} (query {query} bytes, fragment {fragment} bytes)"),
        }
    }
}

fn ipv4_is_local(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local() // includes 169.254.169.254, cloud metadata
        || ip.is_unspecified()
        || ip.is_broadcast()
        || a == 0
        || (a == 100 && (b & 0xc0) == 64) // 100.64.0.0/10, shared address space
}

/// Whether a host *name* is local (SPEC R-LINK.4). A public name that
/// resolves to a local address is caught by the broker, which resolves it
/// before deciding.
pub fn domain_is_local(host: &str) -> bool {
    let host = host.strip_suffix('.').unwrap_or(host);
    let under = |suffix: &str| host == suffix || host.ends_with(&format!(".{suffix}"));
    !host.contains('.')
        || ["localhost", "local", "internal", "lan", "home.arpa"]
            .iter()
            .any(|s| under(s))
}

/// What the human has allowed, as the broker read it for one decision.
#[derive(Debug, Default, Clone)]
pub struct LinkGrants {
    /// The saved grants that apply in this repo: `[open_url] allow` (all
    /// repos) and this repo's `[open_url] repos` entry, parsed. Built by
    /// [`LinkGrants::from_settings`].
    pub always: Vec<HostPattern>,
    /// Allowed for this session: in memory on the MCP path, files bound to
    /// the harness process on the hook path.
    pub session: Vec<HostPattern>,
    /// Sites a human denied this session: refused, never asked again.
    pub denied: Vec<String>,
}

impl LinkGrants {
    /// The grants for one decision in `workspace` (its canonical root, as
    /// `[open_url] repos` keys it): every saved pattern that applies there
    /// ([`OpenUrlSettings::allowed_in`]), plus this session's grants and
    /// denials. An entry that is not a site pattern grants nothing and is
    /// dropped with a warning, as a bad `[network] allow` entry is.
    pub fn from_settings(
        settings: &OpenUrlSettings,
        workspace: &Path,
        session: Vec<HostPattern>,
        denied: Vec<String>,
    ) -> Self {
        let always = settings
            .allowed_in(workspace)
            .into_iter()
            .filter_map(|raw| match HostPattern::parse(raw) {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!(
                        "[open_url] entry '{raw}' is not a site pattern: {e}. It grants \
                         nothing; fix or remove it."
                    );
                    None
                }
            })
            .collect();
        Self {
            always,
            session,
            denied,
        }
    }
}

/// The network restriction in force (SPEC R-LINK.5), whatever the surface:
/// a hooked command reads `[network] restrict` too, though no proxy runs
/// for it.
pub enum Egress<'a> {
    Open,
    /// `allows(host)`: the effective allow list (operator, profiles, session).
    Restricted(&'a dyn Fn(&str) -> bool),
}

/// What to do with one link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkDecision {
    Open,
    Ask(LinkQuestion),
    /// A human said no to this host this session.
    Denied,
}

/// One answer a human may give (SPEC R-LINK.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkChoice {
    Deny,
    /// This link, now. Remembers nothing.
    Once,
    /// This site, for this session (the harness process, or the MCP session).
    Session,
    /// This site, saved for every repo: `[open_url] allow` gains the site.
    /// Of the two "always for this site" answers it is listed first, and is
    /// the default.
    AlwaysSiteAllRepos,
    /// This site, saved for this repo only: this repo's `[open_url] repos`
    /// entry gains the site.
    AlwaysSiteThisRepo,
    /// Any public site, saved for every repo: `[open_url] allow` gains `*`.
    /// There is no per-repo "any site".
    AlwaysAny,
}

impl LinkChoice {
    pub fn label(self) -> &'static str {
        match self {
            LinkChoice::Deny => "deny",
            LinkChoice::Once => "this time",
            LinkChoice::Session => "this site, this session",
            LinkChoice::AlwaysSiteAllRepos => "always for this site — all repos",
            LinkChoice::AlwaysSiteThisRepo => "always for this site — this repo only",
            LinkChoice::AlwaysAny => "always (any site)",
        }
    }
}

/// Where a question is put; each offers what it can keep (SPEC R-LINK.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    /// MCP `elicitation/create` or the TUI, while the command waits for the
    /// answer.
    Live,
    /// The TUI answering a question a hooked command recorded: nothing
    /// waits, so there is no "this time" (R-PERM.10(e)).
    TuiForHarness,
    /// `ahma web`, `ahma desktop`: an answer there may not write settings or
    /// outlive the session (R-GUI.9), so deny, this time, this session only.
    Browser,
}

/// A question about one link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkQuestion {
    pub host: String,
    pub shown: String,
    pub local: bool,
    pub outside_egress: bool,
    pub internationalised: bool,
}

impl LinkQuestion {
    /// The answers `surface` offers, deny first (R-PERM.3.4), then the one
    /// that remembers nothing or least, then "always for this site" for all
    /// repos before this repo only. "Always (any site)" only where it would
    /// cover this link: never for a local host, never for one outside the
    /// egress allow list.
    pub fn choices(&self, surface: Surface) -> Vec<LinkChoice> {
        let any = !self.local && !self.outside_egress;
        let all = match surface {
            Surface::Live => vec![
                LinkChoice::Deny,
                LinkChoice::Once,
                LinkChoice::AlwaysSiteAllRepos,
                LinkChoice::AlwaysSiteThisRepo,
                LinkChoice::AlwaysAny,
            ],
            Surface::TuiForHarness => vec![
                LinkChoice::Deny,
                LinkChoice::Session,
                LinkChoice::AlwaysSiteAllRepos,
                LinkChoice::AlwaysSiteThisRepo,
                LinkChoice::AlwaysAny,
            ],
            Surface::Browser => vec![LinkChoice::Deny, LinkChoice::Once, LinkChoice::Session],
        };
        all.into_iter()
            .filter(|c| any || *c != LinkChoice::AlwaysAny)
            .collect()
    }

    /// The question's body, the same at every surface (SPEC R-LINK.7),
    /// ending with [`GUIDANCE`].
    pub fn text(&self) -> String {
        let mut out = format!(
            "A command wants to open a web link in your browser.\n\nSite: {}\nAddress: {}\n",
            self.host, self.shown
        );
        let notes: Vec<String> = [
            self.local.then(|| LOCAL_NOTE.to_string()),
            self.outside_egress.then(|| outside_egress_note(&self.host)),
            self.internationalised.then(|| IDN_NOTE.to_string()),
        ]
        .into_iter()
        .flatten()
        .collect();
        for note in notes {
            out.push('\n');
            out.push_str(&note);
            out.push('\n');
        }
        out.push('\n');
        out.push_str(GUIDANCE);
        out
    }
}

/// Decide what to do with `target` (SPEC R-LINK.4, R-LINK.5). `resolved_local`:
/// the broker found the name resolves to a local address. Order, as for web
/// domains (R-WEB.2): a saved grant, a session grant, a session deny, then ask.
pub fn decide(
    target: &LinkTarget,
    resolved_local: bool,
    grants: &LinkGrants,
    egress: &Egress<'_>,
) -> LinkDecision {
    let host = target.host();
    let local = target.class() == HostClass::Local || resolved_local;
    // A local host is not egress: loopback is never proxied and the proxy
    // refuses private addresses, so only a host grant of its own covers it.
    let outside_egress = match egress {
        Egress::Open => false,
        Egress::Restricted(allows) => !local && !allows(host),
    };
    let covers = |p: &HostPattern| match p {
        HostPattern::Any => !local && !outside_egress,
        named => named.matches(host),
    };
    if grants.always.iter().chain(&grants.session).any(covers) {
        return LinkDecision::Open;
    }
    if grants.denied.iter().any(|d| d.eq_ignore_ascii_case(host)) {
        return LinkDecision::Denied;
    }
    LinkDecision::Ask(LinkQuestion {
        host: host.to_string(),
        shown: target.shown(),
        local,
        outside_egress,
        internationalised: target.is_internationalised(),
    })
}

/// What an answer does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    Refuse {
        remember_deny: bool,
    },
    OpenOnce,
    Session(HostPattern),
    /// Save the pattern where the binding says
    /// (`ahma_common::open_url::persist_open_url_allow`), then open.
    Persist(HostPattern, OpenUrlBinding),
}

/// The effect of `choice` on the link to `host`, asked from the repo
/// `workspace` (its canonical root, which the broker supplies).
pub fn effect(choice: LinkChoice, host: &str, workspace: &Path) -> Effect {
    let exact = || HostPattern::Exact(host.to_ascii_lowercase());
    match choice {
        LinkChoice::Deny => Effect::Refuse {
            remember_deny: true,
        },
        LinkChoice::Once => Effect::OpenOnce,
        LinkChoice::Session => Effect::Session(exact()),
        LinkChoice::AlwaysSiteAllRepos => Effect::Persist(exact(), OpenUrlBinding::AllRepos),
        LinkChoice::AlwaysSiteThisRepo => {
            Effect::Persist(exact(), OpenUrlBinding::Repo(workspace.to_path_buf()))
        }
        LinkChoice::AlwaysAny => Effect::Persist(HostPattern::Any, OpenUrlBinding::AllRepos),
    }
}

/// The one line a refused link leaves in the command's output (SPEC
/// R-LINK.6, R-PERM.9). `retry_asks`: a hooked command in a harness whose
/// dialog asks (Claude Code); otherwise a human grants it.
pub fn refusal_line(target: &LinkTarget, retry_asks: bool) -> String {
    let host = target.host();
    let grant = format!("`ahma permissions grant open-url {host}`");
    let how = if retry_asks {
        format!(
            "In Claude Code, re-run this command and ahma asks first; elsewhere a human runs \
             {grant}, then re-run."
        )
    } else {
        format!("A human runs {grant}, then re-run.")
    };
    format!(
        "Blocked until a human approves: ahma did not open a link to {host} in the human's \
         browser. {how} Do not open it another way; you may show the human the address: {}",
        target.shown()
    )
}

/// The line for an argument that is not a web link (SPEC R-LINK.2): said,
/// never asked about. A file (what `cargo doc --open` passes) is named so
/// the agent can hand its path to the human.
pub fn not_a_link_line(raw: &str, why: &NotALink) -> String {
    let shown: String = raw.chars().take(200).collect();
    // `has_root`, not `is_absolute`: on Windows `/Users/x` has a root but no drive, and
    // is still a file the human can open.
    let tail = if std::path::Path::new(raw).has_root() || raw.starts_with("file:") {
        " It is a local file: tell the human its path, and they can open it."
    } else {
        " A file or an app is the human's to open."
    };
    format!(
        "ahma opens only http and https links in the human's browser (SPEC R-LINK.2); `{shown}` \
         is {}, so it was not opened.{tail}",
        why.what()
    )
}

/// The link a command line opens when the whole line is `open <URL>` or
/// `xdg-open <URL>` with one http/https URL, bare or quoted, and nothing
/// else (SPEC R-LINK.10). Any other form (a flag, a second word, a pipe, a
/// `&` left unquoted, a variable) is `None` and runs as written.
pub fn exact_open_command(command: &str) -> Option<LinkTarget> {
    let line = command.trim();
    let (program, rest) = line.split_once([' ', '\t'])?;
    if !matches!(
        program,
        "open" | "/usr/bin/open" | "xdg-open" | "/usr/bin/xdg-open"
    ) {
        return None;
    }
    let arg = rest.trim();
    let literal = if let Some(inner) = arg.strip_prefix('\'') {
        let inner = inner.strip_suffix('\'')?;
        (!inner.contains('\'')).then_some(inner)?
    } else if let Some(inner) = arg.strip_prefix('"') {
        let inner = inner.strip_suffix('"')?;
        (!inner.contains(['"', '$', '`', '\\'])).then_some(inner)?
    } else {
        const SHELL: &[char] = &[
            ';', '&', '|', '<', '>', '(', ')', '$', '`', '\\', '"', '\'', '*', '?', '[', ']', '{',
            '}', '~', '#', '!', ' ', '\t', '\n',
        ];
        (!arg.contains(SHELL)).then_some(arg)?
    };
    LinkTarget::parse(literal).ok()
}

/// `s` as one POSIX shell word, for the helper command an exact `open`
/// line runs as.
pub fn sh_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ahma_common::open_url::OpenUrlRepo;
    use std::path::PathBuf;

    fn t(raw: &str) -> LinkTarget {
        LinkTarget::parse(raw).unwrap_or_else(|e| panic!("{raw}: {e:?}"))
    }

    fn pat(s: &str) -> HostPattern {
        HostPattern::parse(s).unwrap()
    }

    #[test]
    fn web_links_parse_and_open_as_their_serialization() {
        let link = t("HTTPS://GitHub.com/ahma-labs/ahma/pull/12");
        assert_eq!(link.host(), "github.com");
        assert_eq!(link.as_str(), "https://github.com/ahma-labs/ahma/pull/12");
        assert_eq!(link.class(), HostClass::Public);
        assert_eq!(t("http://localhost:3000/").class(), HostClass::Local);
        // A path with characters a shell or LaunchServices could read
        // differently is opened percent-encoded.
        assert_eq!(
            t("https://example.com/a\"b<c>").as_str(),
            "https://example.com/a%22b%3Cc%3E"
        );
    }

    #[test]
    fn what_is_not_a_web_link_is_refused_and_says_why() {
        for (raw, why) in [
            ("", NotALink::Empty),
            ("/Users/me/p/target/doc/x/index.html", NotALink::NotWeb),
            ("file:///etc/passwd", NotALink::NotWeb),
            ("x-apple.systempreferences:com.apple", NotALink::NotWeb),
            ("javascript:alert(1)", NotALink::NotWeb),
            ("https:example.com", NotALink::NotWeb), // WHATWG would accept it
            ("https://github.com@evil.example/", NotALink::UserInfo),
            ("https://u:p@example.com/", NotALink::UserInfo),
            ("https://example.com/a b", NotALink::Unsafe(' ')),
            ("https://exa\tmple.com/", NotALink::Unsafe('\t')),
            ("https://evil.example\\@github.com/", NotALink::Unsafe('\\')),
            ("http://[::1]:8000/", NotALink::Ipv6Literal),
        ] {
            assert_eq!(LinkTarget::parse(raw), Err(why), "{raw}");
        }
        let long = format!("https://example.com/{}", "a".repeat(MAX_URL_BYTES));
        assert!(matches!(
            LinkTarget::parse(&long),
            Err(NotALink::TooLong(_))
        ));
    }

    #[test]
    fn the_host_is_the_one_the_parser_reads_not_the_one_the_text_suggests() {
        assert_eq!(t("https://evil.example#@github.com").host(), "evil.example");
        assert_eq!(t("https://evil.example?github.com").host(), "evil.example");
        // WHATWG reads every IPv4 spelling as the address it is.
        assert_eq!(t("http://0x7f.1/").host(), "127.0.0.1");
        assert_eq!(t("http://2130706433/").class(), HostClass::Local);
        assert!(t("https://xn--80ak6aa92e.com/").is_internationalised());
        assert_eq!(t("https://аpple.com/").host(), "xn--pple-43d.com");
    }

    #[test]
    fn local_names_and_addresses_are_local() {
        for raw in [
            "http://localhost/",
            "http://app.localhost/",
            "http://printer.local/",
            "http://intranet/",
            "http://router.lan/",
            "http://10.0.0.1/",
            "http://172.20.1.1/",
            "http://192.168.1.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://100.64.0.1/",
            "http://0.0.0.0:8000/",
        ] {
            assert_eq!(t(raw).class(), HostClass::Local, "{raw}");
        }
        for raw in [
            "https://8.8.8.8/",
            "https://docs.rs/",
            "https://172.32.0.1/",
        ] {
            assert_eq!(t(raw).class(), HostClass::Public, "{raw}");
        }
    }

    #[test]
    fn a_grant_covers_its_host_and_any_covers_only_public_hosts() {
        let any = LinkGrants {
            always: vec![HostPattern::Any],
            ..Default::default()
        };
        assert_eq!(
            decide(&t("https://github.com/x"), false, &any, &Egress::Open),
            LinkDecision::Open
        );
        for (link, resolved_local) in [
            ("http://localhost:3000/", false),
            ("http://192.168.1.1/", false),
            // A public name that resolves to 127.0.0.1 (`*.nip.io`).
            ("http://127.0.0.1.nip.io/", true),
        ] {
            let LinkDecision::Ask(q) = decide(&t(link), resolved_local, &any, &Egress::Open) else {
                panic!("{link}: `*` must not cover a local host");
            };
            assert!(q.local);
            assert!(!q.choices(Surface::Live).contains(&LinkChoice::AlwaysAny));
        }
        let localhost = LinkGrants {
            always: vec![pat("localhost")],
            ..Default::default()
        };
        assert_eq!(
            decide(
                &t("http://localhost:8888/?token=x"),
                false,
                &localhost,
                &Egress::Open
            ),
            LinkDecision::Open,
            "a host grant of its own covers a local host, on every port"
        );
        let wild = LinkGrants {
            always: vec![pat("*.github.io")],
            ..Default::default()
        };
        assert_eq!(
            decide(&t("https://me.github.io/"), false, &wild, &Egress::Open),
            LinkDecision::Open
        );
        assert!(matches!(
            decide(&t("https://evilgithub.io/"), false, &wild, &Egress::Open),
            LinkDecision::Ask(_)
        ));
    }

    #[test]
    fn under_restricted_network_any_covers_only_allowed_hosts() {
        let any = LinkGrants {
            always: vec![HostPattern::Any],
            ..Default::default()
        };
        let allows = |h: &str| h == "github.com";
        let egress = Egress::Restricted(&allows);
        assert_eq!(
            decide(&t("https://github.com/x"), false, &any, &egress),
            LinkDecision::Open
        );
        let LinkDecision::Ask(q) = decide(&t("https://paste.example/x"), false, &any, &egress)
        else {
            panic!("`*` must not carry a link past the egress allow list");
        };
        assert!(q.outside_egress);
        assert!(q.text().contains("not on your network allow list"));
        assert!(!q.choices(Surface::Live).contains(&LinkChoice::AlwaysAny));
        // A host the human named is their decision, egress or not.
        let named = LinkGrants {
            always: vec![pat("paste.example")],
            ..Default::default()
        };
        assert_eq!(
            decide(&t("https://paste.example/x"), false, &named, &egress),
            LinkDecision::Open
        );
    }

    #[test]
    fn a_session_deny_is_not_asked_again_but_a_grant_still_wins() {
        let mut g = LinkGrants {
            denied: vec!["GitHub.com".into()],
            ..Default::default()
        };
        assert_eq!(
            decide(&t("https://github.com/"), false, &g, &Egress::Open),
            LinkDecision::Denied
        );
        g.always.push(pat("github.com"));
        assert_eq!(
            decide(&t("https://github.com/"), false, &g, &Egress::Open),
            LinkDecision::Open
        );
    }

    #[test]
    fn each_surface_offers_what_it_can_keep_deny_first() {
        let LinkDecision::Ask(q) = decide(
            &t("https://github.com/"),
            false,
            &LinkGrants::default(),
            &Egress::Open,
        ) else {
            panic!()
        };
        use LinkChoice::*;
        assert_eq!(
            q.choices(Surface::Live),
            [
                Deny,
                Once,
                AlwaysSiteAllRepos,
                AlwaysSiteThisRepo,
                AlwaysAny
            ]
        );
        assert_eq!(
            q.choices(Surface::TuiForHarness),
            [
                Deny,
                Session,
                AlwaysSiteAllRepos,
                AlwaysSiteThisRepo,
                AlwaysAny
            ]
        );
        // R-GUI.9: a browser surface saves nothing.
        assert_eq!(q.choices(Surface::Browser), [Deny, Once, Session]);
    }

    #[test]
    fn deny_comes_first_and_all_repos_before_this_repo_on_every_surface() {
        // Local and outside-egress questions too, where "any site" drops out.
        let questions = [
            ("https://github.com/", None),
            ("http://localhost:3000/", None),
            ("https://paste.example/", Some("github.com")),
        ];
        for (link, egress_allows) in questions {
            let allows = |h: &str| Some(h) == egress_allows;
            let egress = match egress_allows {
                None => Egress::Open,
                Some(_) => Egress::Restricted(&allows),
            };
            let LinkDecision::Ask(q) = decide(&t(link), false, &LinkGrants::default(), &egress)
            else {
                panic!("{link}")
            };
            for surface in [Surface::Live, Surface::TuiForHarness, Surface::Browser] {
                let c = q.choices(surface);
                assert_eq!(c.first(), Some(&LinkChoice::Deny), "{link} {surface:?}");
                let at = |want| c.iter().position(|x| *x == want);
                match (
                    at(LinkChoice::AlwaysSiteAllRepos),
                    at(LinkChoice::AlwaysSiteThisRepo),
                ) {
                    (Some(all), Some(this)) => {
                        assert!(all < this, "{link} {surface:?}: {c:?}");
                        // Directly after deny and this time / this session.
                        assert_eq!(all, 2, "{link} {surface:?}: {c:?}");
                    }
                    (None, None) => assert_eq!(surface, Surface::Browser),
                    other => panic!("{link} {surface:?}: one site variant alone: {other:?}"),
                }
            }
        }
    }

    #[test]
    fn the_labels_say_site_and_name_which_repos() {
        use LinkChoice::*;
        let labels: Vec<_> = [
            Deny,
            Once,
            Session,
            AlwaysSiteAllRepos,
            AlwaysSiteThisRepo,
            AlwaysAny,
        ]
        .map(LinkChoice::label)
        .into();
        assert_eq!(
            labels,
            [
                "deny",
                "this time",
                "this site, this session",
                "always for this site — all repos",
                "always for this site — this repo only",
                "always (any site)",
            ]
        );
        assert!(labels.iter().all(|l| !l.contains("host")), "{labels:?}");
    }

    #[test]
    fn a_saved_answer_is_bound_to_all_repos_or_to_this_one() {
        let ws = Path::new("/w/a");
        assert_eq!(
            effect(LinkChoice::AlwaysSiteAllRepos, "GitHub.com", ws),
            Effect::Persist(pat("github.com"), OpenUrlBinding::AllRepos)
        );
        assert_eq!(
            effect(LinkChoice::AlwaysSiteThisRepo, "GitHub.com", ws),
            Effect::Persist(pat("github.com"), OpenUrlBinding::Repo(ws.to_path_buf()))
        );
        // "Any site" is all-repos only, whichever repo asked.
        assert_eq!(
            effect(LinkChoice::AlwaysAny, "github.com", ws),
            Effect::Persist(HostPattern::Any, OpenUrlBinding::AllRepos)
        );
        assert_eq!(
            effect(LinkChoice::Session, "GitHub.com", ws),
            Effect::Session(pat("github.com"))
        );
        assert_eq!(effect(LinkChoice::Once, "github.com", ws), Effect::OpenOnce);
        assert_eq!(
            effect(LinkChoice::Deny, "github.com", ws),
            Effect::Refuse {
                remember_deny: true
            }
        );
    }

    #[test]
    fn a_this_repo_grant_opens_in_that_repo_and_asks_in_another() {
        let settings = OpenUrlSettings {
            allow: vec!["docs.rs".into()],
            repos: vec![OpenUrlRepo {
                workspace: PathBuf::from("/w/a"),
                allow: vec!["github.com".into()],
            }],
        };
        let link = t("https://github.com/o/r/pull/1");
        let in_a = LinkGrants::from_settings(&settings, Path::new("/w/a"), vec![], vec![]);
        let in_b = LinkGrants::from_settings(&settings, Path::new("/w/b"), vec![], vec![]);
        assert_eq!(
            decide(&link, false, &in_a, &Egress::Open),
            LinkDecision::Open
        );
        assert!(matches!(
            decide(&link, false, &in_b, &Egress::Open),
            LinkDecision::Ask(_)
        ));
        // An all-repos grant opens everywhere.
        for g in [&in_a, &in_b] {
            assert_eq!(
                decide(&t("https://docs.rs/url"), false, g, &Egress::Open),
                LinkDecision::Open
            );
        }
    }

    #[test]
    fn from_settings_drops_an_entry_that_is_not_a_site_pattern() {
        let settings = OpenUrlSettings {
            allow: vec![
                "https://github.com/o/r".into(), // a URL pasted where a site belongs
                "*.github.io".into(),
            ],
            repos: vec![OpenUrlRepo {
                workspace: PathBuf::from("/w/a"),
                allow: vec!["a*.example.com".into(), "Docs.RS".into()],
            }],
        };
        let g = LinkGrants::from_settings(
            &settings,
            Path::new("/w/a"),
            vec![pat("example.org")],
            vec!["evil.example".into()],
        );
        assert_eq!(g.always, [pat("*.github.io"), pat("docs.rs")]);
        assert_eq!(g.session, [pat("example.org")]);
        assert_eq!(g.denied, ["evil.example"]);
        // The dropped entry grants nothing.
        assert!(matches!(
            decide(&t("https://github.com/o/r"), false, &g, &Egress::Open),
            LinkDecision::Ask(_)
        ));
    }

    #[test]
    fn every_question_carries_the_guidance_and_the_whole_host() {
        let LinkDecision::Ask(q) = decide(
            &t("https://xn--80ak6aa92e.com/?d=1"),
            false,
            &LinkGrants::default(),
            &Egress::Open,
        ) else {
            panic!()
        };
        let text = q.text();
        for must in [
            "Site: xn--80ak6aa92e.com",
            "sends its whole address",
            "signed in",
            "allow only sites you trust",
            "One site is safer than any site",
            "one repo is safer than all repos",
            "never files or apps",
            "ahma permissions revoke open-url",
            "internationalised",
        ] {
            assert!(text.contains(must), "missing {must:?} in:\n{text}");
        }
        assert!(text.ends_with(GUIDANCE), "{text}");
        let long = t(&format!("https://example.com/?q={}", "x".repeat(1000)));
        assert!(
            long.shown().ends_with("more characters)"),
            "{}",
            long.shown()
        );
    }

    #[test]
    fn the_audit_form_keeps_no_query_or_fragment() {
        assert_eq!(
            t("https://github.com/login/device?code=SECRET#frag").audit_form(),
            "https://github.com/login/device (query 11 bytes, fragment 4 bytes)"
        );
        assert_eq!(t("https://docs.rs/url").audit_form(), "https://docs.rs/url");
    }

    #[test]
    fn only_a_whole_line_open_of_one_link_is_a_link_open() {
        for line in [
            "open https://github.com/o/r/pull/1",
            "  open 'https://github.com/o/r/pull/1?a=1&b=2'  ",
            "xdg-open \"https://docs.rs/url\"",
            "/usr/bin/open https://example.com/",
        ] {
            assert!(exact_open_command(line).is_some(), "{line}");
        }
        for line in [
            "open -a Safari https://example.com/",
            "open https://example.com/?a=1&b=2", // `&` backgrounds the open
            "open https://example.com/ && rm -rf x",
            "open https://a.example/ https://b.example/",
            "open \"$URL\"",
            "open ./target/doc/index.html",
            "open file:///etc/passwd",
            "open 'https://x.example/'\"'\"'",
            "gh pr view --web",
            "opener https://example.com/",
        ] {
            assert!(exact_open_command(line).is_none(), "{line}");
        }
        assert_eq!(sh_single_quote("https://x/it's"), r"'https://x/it'\''s'");
    }

    #[test]
    fn a_refusal_says_how_to_allow_it_and_a_file_says_whose_it_is() {
        let line = refusal_line(&t("https://github.com/o/r"), true);
        assert!(
            line.contains("ahma permissions grant open-url github.com"),
            "{line}"
        );
        assert!(line.contains("re-run this command"), "{line}");
        let file = "/Users/me/p/target/doc/crate/index.html";
        let said = not_a_link_line(file, &LinkTarget::parse(file).unwrap_err());
        assert!(said.contains("local file") && said.contains(file), "{said}");
    }
}
