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
     link can make a local service act for you. \"Always for any site\" never covers it.";

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

/// Whether an address a name resolved to is this machine or the local
/// network (SPEC R-LINK.4): what makes the broker treat a public-looking
/// name (`127.0.0.1.nip.io`) as local. IPv6 loopback, unspecified,
/// unique-local and link-local count, and a v4-mapped address is judged as
/// its IPv4 address.
pub fn ip_is_local(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => ipv4_is_local(v4),
        std::net::IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return ipv4_is_local(v4);
            }
            let first = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || (first & 0xfe00) == 0xfc00 // fc00::/7, unique local
                || (first & 0xffc0) == 0xfe80 // fe80::/10, link-local
        }
    }
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

/// Registrable domains whose subdomains belong to different owners though
/// the public suffix list built into ahma does not say so: a wildcard over
/// one would allow anyone's site (SPEC R-LINK.4). Checked against the list
/// first: `githubusercontent.com` is in it, so it is not repeated here.
const SHARED_DOMAINS: &[&str] = &["googleusercontent.com"];

/// The registrable domain `D` of `host` when "`D` and `*.D`" is one owner's
/// (SPEC R-LINK.4): `host` is a DNS name — not an IPv4 address, not local by
/// name, no `xn--` label — whose registrable domain exists (it is not itself a
/// public suffix) and lies under a suffix in the ICANN section of the public
/// suffix list, not its private section of hosts many owners share
/// (`github.io`, `vercel.app`, an S3 bucket's), nor in ahma's own short list
/// of such domains the list does not mark (`googleusercontent.com`). The
/// list is the `psl` crate's compiled-in snapshot: a multi-tenant host added
/// after it is read as an ordinary domain.
pub fn domain_family(host: &str) -> Option<String> {
    let host = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
    if host.parse::<Ipv4Addr>().is_ok()
        || domain_is_local(&host)
        || host.split('.').any(|l| l.starts_with("xn--"))
    {
        return None;
    }
    let domain = psl::domain(host.as_bytes())?;
    if domain.suffix().typ() != Some(psl::Type::Icann) {
        return None;
    }
    let d = std::str::from_utf8(domain.as_bytes()).ok()?.to_string();
    (!SHARED_DOMAINS.contains(&d.as_str())).then_some(d)
}

/// Whether `name` is a suffix many owners share: in the public suffix list
/// (either section) or in [`SHARED_DOMAINS`].
fn is_shared_suffix(name: &str) -> bool {
    SHARED_DOMAINS.contains(&name)
        || psl::suffix(name.as_bytes())
            .is_some_and(|s| s.is_known() && s.as_bytes() == name.as_bytes())
}

/// The registrable domain `ahma permissions grant open-url SITE
/// --and-subdomains` writes with its wildcard (SPEC R-LINK.11): `SITE`
/// itself, when it is a registrable domain [`domain_family`] offers. `Err`
/// says why not, naming the right site where there is one.
pub fn and_subdomains(site: &str) -> Result<String, String> {
    let site = site.to_ascii_lowercase();
    if site.contains('*') {
        return Err(format!(
            "--and-subdomains takes a site name (`github.com`), not a pattern: `{site}`"
        ));
    }
    if site.parse::<Ipv4Addr>().is_ok()
        || domain_is_local(&site)
        || site.split('.').any(|l| l.starts_with("xn--"))
    {
        return Err(format!(
            "--and-subdomains is not offered for an IP address, a local name or an \
             internationalised (`xn--`) name; grant `{site}` alone"
        ));
    }
    match domain_family(&site) {
        Some(d) if d == site => Ok(d),
        Some(d) => Err(format!(
            "`{site}` is not a registrable domain; for it and its sibling subdomains use \
             `{d} --and-subdomains`, or grant `{site}` alone"
        )),
        None if psl::suffix(site.as_bytes()).is_some_and(|s| s.is_known())
            || SHARED_DOMAINS.iter().any(|d| site.ends_with(d)) =>
        {
            Err(format!(
                "`{site}` is under a suffix many owners share (the public suffix list's private \
                 section, as `github.io` and `vercel.app` are), so its subdomains are other \
                 people's sites; grant one site by name instead (`someone.github.io`)"
            ))
        }
        None => Err(format!(
            "`{site}` is not under a public suffix ahma knows, so it cannot tell whether one \
             owner holds its subdomains; grant `{site}` alone"
        )),
    }
}

/// The warning a grant of `pattern` carries when it is a wildcard over a
/// suffix many owners share (`*.github.io`): it allows anyone's site there
/// (SPEC R-LINK.11). `None` for any other pattern.
pub fn shared_wildcard_warning(pattern: &str) -> Option<String> {
    let suffix = pattern.strip_prefix("*.")?.to_ascii_lowercase();
    is_shared_suffix(&suffix).then(|| {
        format!(
            "{pattern} allows anyone's site on {suffix}: many owners publish sites there. Naming \
             one site (`someone.{suffix}`) is safer."
        )
    })
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

    /// Whether a saved or session grant opens links to `host` (SPEC R-LINK.4,
    /// R-LINK.5): a grant naming it does; "any site" only when the host is
    /// neither `local` nor `outside_egress`.
    pub fn covers(&self, host: &str, local: bool, outside_egress: bool) -> bool {
        self.covers_saved(host, local, outside_egress)
            || self.covers_session(host, local, outside_egress)
    }

    /// Whether a saved grant (`[open_url]`) opens links to `host`.
    pub fn covers_saved(&self, host: &str, local: bool, outside_egress: bool) -> bool {
        Self::any_covers(&self.always, host, local, outside_egress)
    }

    /// Whether a grant made this session opens links to `host`.
    pub fn covers_session(&self, host: &str, local: bool, outside_egress: bool) -> bool {
        Self::any_covers(&self.session, host, local, outside_egress)
    }

    fn any_covers(patterns: &[HostPattern], host: &str, local: bool, outside_egress: bool) -> bool {
        patterns.iter().any(|p| match p {
            HostPattern::Any => !local && !outside_egress,
            named => named.matches(host),
        })
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
///
/// The narrowest lasting answer is this site in this repo; every other
/// lasting answer widens it on exactly one axis — every repo, or any site.
/// A grant wider on both is given only with the CLI (R-LINK.11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkChoice {
    Deny,
    /// This link, now. Remembers nothing.
    Once,
    /// This site, for this session (the harness process, or the MCP session).
    Session,
    /// This site, saved for this repo only: this repo's `[open_url] repos`
    /// entry gains the site. The first lasting answer, and the usual one.
    AlwaysSiteThisRepo,
    /// This site, saved for every repo: `[open_url] allow` gains the site.
    AlwaysSiteAllRepos,
    /// This site's registrable domain and its one-label subdomains, saved
    /// for this repo only: `D` and `*.D` ([`LinkQuestion::domain`]). Offered
    /// only where one owner holds `D` (R-LINK.4).
    AlwaysDomainThisRepo,
    /// Any public site, saved for this repo only: this repo's `[open_url]
    /// repos` entry gains `*`. Any site for every repo is CLI-only.
    AlwaysAnyThisRepo,
}

impl LinkChoice {
    /// Every answer, in the order surfaces offer them.
    pub const ALL: [LinkChoice; 7] = [
        LinkChoice::Deny,
        LinkChoice::Once,
        LinkChoice::Session,
        LinkChoice::AlwaysSiteThisRepo,
        LinkChoice::AlwaysSiteAllRepos,
        LinkChoice::AlwaysDomainThisRepo,
        LinkChoice::AlwaysAnyThisRepo,
    ];

    /// The value a form sends for this answer (an elicitation's single
    /// select, SPEC R-PERM.3.4): stable, never shown. A value an older build
    /// sent with another meaning (`always-site` was every repo,
    /// `always-any-site` any site in every repo) is no longer one of these,
    /// so it reads as a deny ([`Self::from_value`]).
    pub fn value(self) -> &'static str {
        match self {
            LinkChoice::Deny => "deny",
            LinkChoice::Once => "once",
            LinkChoice::Session => "session",
            LinkChoice::AlwaysSiteThisRepo => "always-site-this-repo",
            LinkChoice::AlwaysSiteAllRepos => "always-site-all-repos",
            LinkChoice::AlwaysDomainThisRepo => "always-domain-this-repo",
            LinkChoice::AlwaysAnyThisRepo => "always-any-site-this-repo",
        }
    }

    /// The answer a form sent, when it is one of `offered`; anything else —
    /// a misspelling, an empty value, an answer this question did not offer —
    /// is a deny: an answer ahma cannot read is not consent (SPEC R5.3.1).
    pub fn from_value(raw: &str, offered: &[LinkChoice]) -> LinkChoice {
        let raw = raw.trim();
        offered
            .iter()
            .copied()
            .find(|c| c.value().eq_ignore_ascii_case(raw))
            .unwrap_or(LinkChoice::Deny)
    }

    /// The answer in words, naming no site ([`LinkQuestion::label`] names
    /// it).
    pub fn label(self) -> &'static str {
        match self {
            LinkChoice::Deny => "deny",
            LinkChoice::Once => "this time",
            LinkChoice::Session => "this site, this session",
            LinkChoice::AlwaysSiteThisRepo => "always for this site — this repo only",
            LinkChoice::AlwaysSiteAllRepos => "always for this site — all repos",
            LinkChoice::AlwaysDomainThisRepo => {
                "always for this site and its subdomains — this repo only"
            }
            LinkChoice::AlwaysAnyThisRepo => "always for any site — this repo only",
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
    /// For a link a hooked command was refused, which nothing waits on, that
    /// leaves deny and this session: what both this and [`Self::TuiForHarness`]
    /// offer (`ahma_tui::harness_asks::HarnessLinkAsk::unsaved_choices`).
    Browser,
    /// Claude Code's own dialog, before a hooked command whose link was
    /// refused runs again. Its contract is yes or no: no is deny, yes is
    /// "this time" (the approved run may open the site's links), and the
    /// lasting answers are named as the grants a human runs
    /// ([`harness_dialog_answers`]).
    HarnessDialog,
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
    /// The question a link a hooked command was refused asks, recorded for
    /// the harness dialog and the TUI (SPEC R-LINK.6): the same at every
    /// surface that reads the record.
    pub fn from_refusal(refusal: &ahma_common::harness_asks::LinkRefusal) -> Self {
        Self {
            host: refusal.host.to_ascii_lowercase(),
            shown: refusal.shown.clone(),
            local: refusal.local,
            outside_egress: refusal.outside_egress,
            internationalised: refusal.host.split('.').any(|l| l.starts_with("xn--")),
        }
    }

    /// The answers `surface` offers, deny first (R-PERM.3.4), then the one
    /// that remembers nothing or least, then the lasting ones, narrowest
    /// first: this site in this repo (the usual one), this site in every
    /// repo, this site's domain and its subdomains in this repo — only where
    /// [`Self::domain`] names one — then any site in this repo — only where
    /// it would cover this link: never for a local host, never for one
    /// outside the egress allow list. At most six.
    pub fn choices(&self, surface: Surface) -> Vec<LinkChoice> {
        let any = !self.local && !self.outside_egress;
        let domain = self.domain().is_some();
        let all = match surface {
            Surface::Live | Surface::HarnessDialog => vec![
                LinkChoice::Deny,
                LinkChoice::Once,
                LinkChoice::AlwaysSiteThisRepo,
                LinkChoice::AlwaysSiteAllRepos,
                LinkChoice::AlwaysDomainThisRepo,
                LinkChoice::AlwaysAnyThisRepo,
            ],
            Surface::TuiForHarness => vec![
                LinkChoice::Deny,
                LinkChoice::Session,
                LinkChoice::AlwaysSiteThisRepo,
                LinkChoice::AlwaysSiteAllRepos,
                LinkChoice::AlwaysDomainThisRepo,
                LinkChoice::AlwaysAnyThisRepo,
            ],
            Surface::Browser => vec![LinkChoice::Deny, LinkChoice::Once, LinkChoice::Session],
        };
        all.into_iter()
            .filter(|c| match c {
                LinkChoice::AlwaysAnyThisRepo => any,
                LinkChoice::AlwaysDomainThisRepo => domain,
                _ => true,
            })
            .collect()
    }

    /// The registrable domain "this site and its subdomains" would grant
    /// (`D`, written with `*.D`), when it is offered for this question (SPEC
    /// R-LINK.4): never for a local host, an internationalised name or a site
    /// outside the egress allow list, and otherwise as [`domain_family`] says.
    pub fn domain(&self) -> Option<String> {
        if self.local || self.outside_egress || self.internationalised {
            return None;
        }
        domain_family(&self.host)
    }

    /// How every surface shows `choice` for this question: the answer in
    /// words with the site it names (`Always for github.com — this repo
    /// only`), so the human sees what would be saved.
    pub fn label(&self, choice: LinkChoice) -> String {
        let host = &self.host;
        match choice {
            LinkChoice::AlwaysSiteThisRepo => format!("Always for {host} — this repo only"),
            LinkChoice::AlwaysSiteAllRepos => format!("Always for {host} — all repos"),
            LinkChoice::AlwaysDomainThisRepo => match self.domain() {
                Some(d) => format!("Always for {d} and *.{d} — this repo only"),
                None => capitalised(choice.label()),
            },
            other => capitalised(other.label()),
        }
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
    // One order for every permission (SPEC R-POLICY.2): a session answer, then
    // a standing permit. A grant withdraws this session's deny: it is the later
    // answer (a denied site is never asked again, so a grant can only come
    // after), and the deny's refusal line names exactly that grant as the way
    // through (R-LINK.6). So a deny is reported only while no grant covers the
    // site.
    use ahma_common::policy::{
        self, Action, Decision, DefaultAnswer, Facts, Resource, SessionAnswer,
    };
    let saved = grants.covers_saved(host, local, outside_egress);
    let mut facts = Facts::new(DefaultAnswer::Ask);
    facts.session = if grants.covers_session(host, local, outside_egress) {
        Some(SessionAnswer::Granted)
    } else if !saved && grants.denied.iter().any(|d| d.eq_ignore_ascii_case(host)) {
        Some(SessionAnswer::Denied)
    } else {
        None
    };
    facts.permitted = saved.then(|| "[open_url]".to_string());
    let host_lc = host.to_ascii_lowercase();
    match policy::decide(
        &policy::Request::new(Action::OpenLink, Resource::Host(&host_lc)),
        &facts,
    ) {
        Decision::Allow { .. } => return LinkDecision::Open,
        Decision::Deny { .. } => return LinkDecision::Denied,
        Decision::Ask => {}
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
    /// Save the patterns — one, or a domain and its one-label wildcard —
    /// where the binding says, in one write
    /// (`ahma_common::open_url::persist_open_url_allows`), then open.
    Persist(Vec<HostPattern>, OpenUrlBinding),
}

/// The effect of `choice` on the link to `host`, asked from the repo
/// `workspace` (its canonical root, which the broker supplies).
pub fn effect(choice: LinkChoice, host: &str, workspace: &Path) -> Effect {
    let exact = || HostPattern::Exact(host.to_ascii_lowercase());
    let here = || OpenUrlBinding::Repo(workspace.to_path_buf());
    match choice {
        LinkChoice::Deny => Effect::Refuse {
            remember_deny: true,
        },
        LinkChoice::Once => Effect::OpenOnce,
        LinkChoice::Session => Effect::Session(exact()),
        LinkChoice::AlwaysSiteThisRepo => Effect::Persist(vec![exact()], here()),
        LinkChoice::AlwaysSiteAllRepos => Effect::Persist(vec![exact()], OpenUrlBinding::AllRepos),
        // Only ever offered with a domain; without one it widens nothing.
        LinkChoice::AlwaysDomainThisRepo => match domain_family(host) {
            Some(d) => Effect::Persist(
                vec![HostPattern::Exact(d.clone()), HostPattern::Wildcard(d)],
                here(),
            ),
            None => Effect::Refuse {
                remember_deny: false,
            },
        },
        LinkChoice::AlwaysAnyThisRepo => Effect::Persist(vec![HostPattern::Any], here()),
    }
}

/// The `ahma permissions grant open-url` line that saves what `choice`
/// would (SPEC R-LINK.11): `None` for an answer that saves nothing. It names
/// its binding (`--workspace '<repo>'` or `--all-repos`), so it means the same
/// whatever directory a human runs it in.
pub fn grant_command(choice: LinkChoice, host: &str, workspace: &Path) -> Option<String> {
    let Effect::Persist(patterns, binding) = effect(choice, host, workspace) else {
        return None;
    };
    let mut line = String::from("ahma permissions grant open-url");
    if let Some(HostPattern::Exact(site)) = patterns.first() {
        line.push(' ');
        line.push_str(site);
    }
    if patterns.len() > 1 {
        line.push_str(" --and-subdomains");
    }
    match &binding {
        OpenUrlBinding::Repo(repo) => {
            line.push_str(" --workspace ");
            line.push_str(&sh_single_quote(&repo.to_string_lossy()));
        }
        OpenUrlBinding::AllRepos => line.push_str(" --all-repos"),
    }
    Some(line)
}

/// Every answer to `question` in Claude Code's dialog ([`Surface::HarnessDialog`]),
/// one line each, in the order [`LinkQuestion::choices`] gives (SPEC R-LINK.6):
/// how the human gives it there. Its yes/no answers deny and this time; a
/// lasting one is the grant a human runs, then re-runs the command.
/// `workspace` is the repo the question was asked from.
pub fn harness_dialog_answers(question: &LinkQuestion, workspace: &Path) -> Vec<String> {
    let host = &question.host;
    question
        .choices(Surface::HarnessDialog)
        .into_iter()
        .filter_map(|choice| {
            let label = choice.label();
            let shown = question.label(choice);
            Some(match choice {
                LinkChoice::Deny => format!(
                    "No — {label}: ahma does not ask about {host} again in this Claude Code \
                     session."
                ),
                LinkChoice::Once => format!(
                    "Yes — {label}: this run of the command may open {host} links; nothing is \
                     remembered."
                ),
                // The dialog keeps nothing for the session (its yes is "this time").
                LinkChoice::Session => return None,
                lasting => {
                    let usual = if lasting == LinkChoice::AlwaysSiteThisRepo {
                        " (the usual lasting answer)"
                    } else {
                        ""
                    };
                    let grant = grant_command(lasting, host, workspace)?;
                    format!("{shown}{usual}: a human runs `{grant}`.")
                }
            })
        })
        .collect()
}

fn capitalised(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The one line a refused link leaves in the command's output (SPEC
/// R-LINK.6, R-PERM.9). `retry_asks`: a hooked command in a harness whose
/// dialog asks (Claude Code); otherwise a human grants it.
pub fn refusal_line(target: &LinkTarget, retry_asks: bool) -> String {
    refused(target, retry_asks, None)
}

/// The line for a link nobody could be asked about while the command waited
/// (SPEC R-LINK.6, rung 3 of R-PERM.3): it names the grant, and says why the
/// question was not put, so the human knows where it did not go (R-PERM.3.2).
pub fn unasked_refusal_line(target: &LinkTarget, why: &str) -> String {
    refused(target, false, Some(why))
}

fn refused(target: &LinkTarget, retry_asks: bool, unasked: Option<&str>) -> String {
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
    let unasked = unasked
        .map(|why| format!(", and could not ask: {why}"))
        .unwrap_or_default();
    format!(
        "Blocked until a human approves: ahma did not open a link to {host} in the human's \
         browser{unasked}. {how} Do not open it another way; you may show the human the \
         address: {}",
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
    LinkTarget::parse(one_literal_word(rest)?).ok()
}

/// `arg` as the one word a shell passes, when it is a literal: bare without
/// a shell metacharacter, or single- or double-quoted with nothing inside
/// that the shell would expand or end the quote with. `None` for anything
/// else (two words, a variable, an operator).
fn one_literal_word(arg: &str) -> Option<&str> {
    let arg = arg.trim();
    if let Some(inner) = arg.strip_prefix('\'') {
        let inner = inner.strip_suffix('\'')?;
        (!inner.contains('\'')).then_some(inner)
    } else if let Some(inner) = arg.strip_prefix('"') {
        let inner = inner.strip_suffix('"')?;
        (!inner.contains(['"', '$', '`', '\\'])).then_some(inner)
    } else {
        const SHELL: &[char] = &[
            ';', '&', '|', '<', '>', '(', ')', '$', '`', '\\', '"', '\'', '*', '?', '[', ']', '{',
            '}', '~', '#', '!', ' ', '\t', '\n',
        ];
        (!arg.contains(SHELL)).then_some(arg)
    }
}

/// The site a command line opens a link to, when the whole line opens one
/// literal http/https link: a whole-line `open` ([`exact_open_command`]),
/// `ahma open-url <URL>`, or `$BROWSER` / `$GH_BROWSER` (quoted or not) with
/// one URL. What lets the harness dialog ask about a refused site before a
/// command that opens it again, and never before an unrelated one (SPEC
/// R-LINK.6).
pub fn link_open_host(command: &str) -> Option<String> {
    if let Some(target) = exact_open_command(command) {
        return Some(target.host().to_string());
    }
    let (program, rest) = command.trim().split_once([' ', '\t'])?;
    let arg = match program {
        "$BROWSER" | "\"$BROWSER\"" | "$GH_BROWSER" | "\"$GH_BROWSER\"" => rest,
        _ if Path::new(program).file_name() == Some(std::ffi::OsStr::new("ahma")) => {
            let (sub, arg) = rest.trim_start().split_once([' ', '\t'])?;
            (sub == super::helper::SUBCOMMAND).then_some(arg)?
        }
        _ => return None,
    };
    LinkTarget::parse(one_literal_word(arg)?)
        .ok()
        .map(|t| t.host().to_string())
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
            assert!(
                !q.choices(Surface::Live)
                    .contains(&LinkChoice::AlwaysAnyThisRepo)
            );
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
            always: vec![pat("*.github.com")],
            ..Default::default()
        };
        assert_eq!(
            decide(&t("https://api.github.com/"), false, &wild, &Egress::Open),
            LinkDecision::Open
        );
        for other in ["https://evilgithub.com/", "https://github.com/"] {
            assert!(
                matches!(
                    decide(&t(other), false, &wild, &Egress::Open),
                    LinkDecision::Ask(_)
                ),
                "{other}: one label, not the domain itself"
            );
        }
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
        assert!(
            !q.choices(Surface::Live)
                .contains(&LinkChoice::AlwaysAnyThisRepo)
        );
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
                AlwaysSiteThisRepo,
                AlwaysSiteAllRepos,
                AlwaysDomainThisRepo,
                AlwaysAnyThisRepo
            ]
        );
        assert_eq!(
            q.choices(Surface::TuiForHarness),
            [
                Deny,
                Session,
                AlwaysSiteThisRepo,
                AlwaysSiteAllRepos,
                AlwaysDomainThisRepo,
                AlwaysAnyThisRepo
            ]
        );
        // R-GUI.9: a browser surface saves nothing.
        assert_eq!(q.choices(Surface::Browser), [Deny, Once, Session]);
        // Claude Code's dialog: deny, this time, then the lasting answers.
        assert_eq!(
            q.choices(Surface::HarnessDialog),
            [
                Deny,
                Once,
                AlwaysSiteThisRepo,
                AlwaysSiteAllRepos,
                AlwaysDomainThisRepo,
                AlwaysAnyThisRepo
            ]
        );
    }

    /// SPEC R-LINK.6, R-LINK.11: Claude Code's dialog is yes or no, so it
    /// says what each answer is there, in order: no denies, yes is this time,
    /// and each lasting answer is the exact grant a human runs, naming its
    /// binding — this repo first, then all repos, then any site in this repo
    /// only where it would cover the link.
    #[test]
    fn the_harness_dialog_names_every_answer_in_order() {
        let ws = Path::new("/w/my repo");
        let LinkDecision::Ask(q) = decide(
            &t("https://github.com/o/r/pull/1"),
            false,
            &LinkGrants::default(),
            &Egress::Open,
        ) else {
            panic!()
        };
        let answers = harness_dialog_answers(&q, ws);
        assert_eq!(answers.len(), 6, "{answers:#?}");
        assert!(answers[0].starts_with("No — deny"), "{}", answers[0]);
        assert!(answers[1].starts_with("Yes — this time"), "{}", answers[1]);
        assert!(
            answers[2].starts_with("Always for github.com — this repo only (the usual")
                && answers[2].contains(
                    "`ahma permissions grant open-url github.com --workspace '/w/my repo'`"
                ),
            "{}",
            answers[2]
        );
        assert!(
            answers[3].starts_with("Always for github.com — all repos:")
                && answers[3].contains("`ahma permissions grant open-url github.com --all-repos`"),
            "{}",
            answers[3]
        );
        assert!(
            answers[4].starts_with("Always for github.com and *.github.com — this repo only")
                && answers[4].contains(
                    "`ahma permissions grant open-url github.com --and-subdomains --workspace \
                     '/w/my repo'`"
                ),
            "{}",
            answers[4]
        );
        assert!(
            answers[5].starts_with("Always for any site — this repo only")
                && answers[5]
                    .contains("`ahma permissions grant open-url --workspace '/w/my repo'`"),
            "{}",
            answers[5]
        );
        let LinkDecision::Ask(local) = decide(
            &t("http://localhost:3000/"),
            false,
            &LinkGrants::default(),
            &Egress::Open,
        ) else {
            panic!()
        };
        let answers = harness_dialog_answers(&local, ws);
        assert_eq!(
            answers.len(),
            4,
            "no domain or any-site answer for a local host"
        );
        assert!(
            answers[3].contains("grant open-url localhost --all-repos`"),
            "{answers:#?}"
        );
        assert_eq!(grant_command(LinkChoice::Once, "github.com", ws), None);
    }

    /// SPEC R-LINK.6: the dialog asks about a refused site before a command
    /// that opens it again — a whole-line `open`, `ahma open-url`, or
    /// `$BROWSER` / `$GH_BROWSER` of one literal link — and names no site for
    /// anything else.
    #[test]
    fn a_helper_command_names_the_host_it_opens() {
        for (line, host) in [
            ("open https://github.com/o/r/pull/1", "github.com"),
            ("ahma open-url 'https://GitHub.com/x?a=1&b=2'", "github.com"),
            (
                "/usr/local/bin/ahma open-url https://docs.rs/url",
                "docs.rs",
            ),
            ("\"$BROWSER\" 'https://example.com/x'", "example.com"),
            ("$GH_BROWSER https://github.com/login/device", "github.com"),
        ] {
            assert_eq!(link_open_host(line).as_deref(), Some(host), "{line}");
        }
        for line in [
            "gh pr view --web",
            "ahma open-url https://a.example/ https://b.example/",
            "ahma open-url \"$URL\"",
            "\"$BROWSER\" https://x.example/?a=1&b=2",
            "\"$BROWSER\" /w/a/target/doc/index.html",
            "ahma permissions grant open-url github.com",
            "echo https://github.com/",
            "notahma open-url https://github.com/",
        ] {
            assert_eq!(link_open_host(line), None, "{line}");
        }
    }

    fn ask(link: &str) -> LinkQuestion {
        match decide(&t(link), false, &LinkGrants::default(), &Egress::Open) {
            LinkDecision::Ask(q) => q,
            other => panic!("{link}: {other:?}"),
        }
    }

    /// SPEC R-LINK.6, R-LINK.4: the three example links, on every surface.
    /// The first lasting answer is this site in this repo; each other one
    /// widens it on one axis. A site and its subdomains is offered for
    /// `github.com` and `api.github.com` (both `github.com` and
    /// `*.github.com`), never for `someone.github.io`, whose suffix many
    /// owners share; any site is for this repo only.
    #[test]
    fn the_three_examples_offer_this_repo_first() {
        use LinkChoice::*;
        let ws = Path::new("/Users/me/proj");
        let grant = |args: &str| format!("a human runs `ahma permissions grant open-url {args}`.");
        for (link, host, domain) in [
            (
                "https://github.com/o/r/pull/1",
                "github.com",
                Some("github.com"),
            ),
            (
                "https://api.github.com/x",
                "api.github.com",
                Some("github.com"),
            ),
            ("https://someone.github.io/page", "someone.github.io", None),
        ] {
            let q = ask(link);
            assert_eq!(q.domain().as_deref(), domain, "{link}");
            let with_domain = |mut v: Vec<LinkChoice>| {
                if domain.is_some() {
                    v.insert(v.len() - 1, AlwaysDomainThisRepo);
                }
                v
            };
            assert_eq!(
                q.choices(Surface::Live),
                with_domain(vec![
                    Deny,
                    Once,
                    AlwaysSiteThisRepo,
                    AlwaysSiteAllRepos,
                    AlwaysAnyThisRepo
                ]),
                "{link}"
            );
            assert_eq!(
                q.choices(Surface::TuiForHarness),
                with_domain(vec![
                    Deny,
                    Session,
                    AlwaysSiteThisRepo,
                    AlwaysSiteAllRepos,
                    AlwaysAnyThisRepo
                ]),
                "{link}"
            );
            assert_eq!(q.choices(Surface::Browser), [Deny, Once, Session]);
            assert!(q.choices(Surface::Live).len() <= 6);
            let titles: Vec<String> = q
                .choices(Surface::Live)
                .into_iter()
                .map(|c| q.label(c))
                .collect();
            let mut want = vec![
                "Deny".to_string(),
                "This time".into(),
                format!("Always for {host} — this repo only"),
                format!("Always for {host} — all repos"),
            ];
            want.extend(domain.map(|d| format!("Always for {d} and *.{d} — this repo only")));
            want.push("Always for any site — this repo only".into());
            assert_eq!(titles, want, "{link}");

            let dialog = harness_dialog_answers(&q, ws);
            let mut want = vec![
                format!(
                    "Always for {host} — this repo only (the usual lasting answer): {}",
                    grant(&format!("{host} --workspace '/Users/me/proj'"))
                ),
                format!(
                    "Always for {host} — all repos: {}",
                    grant(&format!("{host} --all-repos"))
                ),
            ];
            want.extend(domain.map(|d| {
                format!(
                    "Always for {d} and *.{d} — this repo only: {}",
                    grant(&format!(
                        "{d} --and-subdomains --workspace '/Users/me/proj'"
                    ))
                )
            }));
            want.push(format!(
                "Always for any site — this repo only: {}",
                grant("--workspace '/Users/me/proj'")
            ));
            assert_eq!(dialog[2..], want, "{link}");
        }
    }

    /// SPEC R-LINK.4: "a site and its subdomains" writes the registrable
    /// domain and its one-label wildcard, for this repo; the wildcard covers
    /// one label only, as every pattern does.
    #[test]
    fn a_site_and_its_subdomains_is_two_patterns_for_this_repo() {
        let ws = Path::new("/w/a");
        assert_eq!(
            effect(LinkChoice::AlwaysDomainThisRepo, "API.github.com", ws),
            Effect::Persist(
                vec![pat("github.com"), pat("*.github.com")],
                OpenUrlBinding::Repo(ws.to_path_buf())
            )
        );
        let g = LinkGrants {
            always: vec![pat("github.com"), pat("*.github.com")],
            ..Default::default()
        };
        for (link, opens) in [
            ("https://github.com/", true),
            ("https://gist.github.com/", true),
            ("https://a.b.github.com/", false),
            ("https://evilgithub.com/", false),
        ] {
            assert_eq!(
                decide(&t(link), false, &g, &Egress::Open) == LinkDecision::Open,
                opens,
                "{link}"
            );
        }
        // Never written for a host it is not offered for: a deny that is not
        // remembered, so nothing is widened.
        assert_eq!(
            effect(LinkChoice::AlwaysDomainThisRepo, "someone.github.io", ws),
            Effect::Refuse {
                remember_deny: false
            }
        );
    }

    /// SPEC R-LINK.4: the domain answer is offered only for a public DNS
    /// name whose registrable domain is under an ICANN suffix — never a
    /// suffix many owners share, an IP address, a local or internationalised
    /// name, or a site outside the egress allow list.
    #[test]
    fn a_domain_family_only_where_one_owner_holds_it() {
        for (host, want) in [
            ("github.com", Some("github.com")),
            ("api.github.com", Some("github.com")),
            ("a.b.example.co.uk", Some("example.co.uk")),
            ("docs.rs", Some("docs.rs")),
            ("someone.github.io", None),
            ("my-app.vercel.app", None),
            ("x.herokuapp.com", None),
            ("bucket.s3.amazonaws.com", None),
            ("raw.githubusercontent.com", None),
            ("lh3.googleusercontent.com", None),
            ("github.io", None),
            ("co.uk", None),
            ("8.8.8.8", None),
            ("localhost", None),
            ("printer.local", None),
            ("xn--80ak6aa92e.com", None),
            ("host.unknowntld", None),
        ] {
            assert_eq!(domain_family(host).as_deref(), want, "{host}");
        }
        let allows = |h: &str| h == "github.com";
        let LinkDecision::Ask(outside) = decide(
            &t("https://api.example.com/"),
            false,
            &LinkGrants::default(),
            &Egress::Restricted(&allows),
        ) else {
            panic!()
        };
        assert_eq!(outside.domain(), None, "outside the egress list");
        assert_eq!(
            outside.choices(Surface::Live),
            [
                LinkChoice::Deny,
                LinkChoice::Once,
                LinkChoice::AlwaysSiteThisRepo,
                LinkChoice::AlwaysSiteAllRepos
            ]
        );
        let LinkDecision::Ask(resolved_local) = decide(
            &t("http://127.0.0.1.nip.io/"),
            true,
            &LinkGrants::default(),
            &Egress::Open,
        ) else {
            panic!()
        };
        assert_eq!(
            resolved_local.domain(),
            None,
            "a name that resolves locally"
        );
    }

    /// SPEC R-LINK.11: `--and-subdomains` takes a registrable domain the
    /// domain answer would be offered for, and names the right one otherwise.
    #[test]
    fn and_subdomains_takes_only_a_registrable_domain_one_owner_holds() {
        assert_eq!(and_subdomains("GitHub.com"), Ok("github.com".to_string()));
        for (site, says) in [
            ("api.github.com", "github.com --and-subdomains"),
            ("github.io", "many owners"),
            ("someone.github.io", "many owners"),
            ("*.github.com", "a site name"),
            ("localhost", "not offered"),
            ("8.8.8.8", "not offered"),
            ("host.unknowntld", "not under a public suffix"),
        ] {
            let err = and_subdomains(site).expect_err(site);
            assert!(err.contains(says), "{site}: {err}");
        }
        assert!(shared_wildcard_warning("*.github.io").is_some());
        assert!(shared_wildcard_warning("*.vercel.app").is_some());
        assert!(shared_wildcard_warning("*.googleusercontent.com").is_some());
        assert!(shared_wildcard_warning("*.github.com").is_none());
        assert!(shared_wildcard_warning("github.io").is_none(), "one site");
    }

    #[test]
    fn deny_comes_first_and_this_repo_before_all_repos_on_every_surface() {
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
            for surface in [
                Surface::Live,
                Surface::TuiForHarness,
                Surface::Browser,
                Surface::HarnessDialog,
            ] {
                let c = q.choices(surface);
                assert_eq!(c.first(), Some(&LinkChoice::Deny), "{link} {surface:?}");
                let at = |want| c.iter().position(|x| *x == want);
                match (
                    at(LinkChoice::AlwaysSiteThisRepo),
                    at(LinkChoice::AlwaysSiteAllRepos),
                ) {
                    (Some(this), Some(all)) => {
                        assert!(this < all, "{link} {surface:?}: {c:?}");
                        // Directly after deny and this time / this session.
                        assert_eq!(this, 2, "{link} {surface:?}: {c:?}");
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
        let labels: Vec<_> = LinkChoice::ALL.map(LinkChoice::label).into();
        assert_eq!(
            labels,
            [
                "deny",
                "this time",
                "this site, this session",
                "always for this site — this repo only",
                "always for this site — all repos",
                "always for this site and its subdomains — this repo only",
                "always for any site — this repo only",
            ]
        );
        assert_eq!(
            LinkChoice::ALL[3..],
            [
                AlwaysSiteThisRepo,
                AlwaysSiteAllRepos,
                AlwaysDomainThisRepo,
                AlwaysAnyThisRepo
            ]
        );
        assert!(labels.iter().all(|l| !l.contains("host")), "{labels:?}");
    }

    #[test]
    fn a_saved_answer_is_bound_to_all_repos_or_to_this_one() {
        let ws = Path::new("/w/a");
        assert_eq!(
            effect(LinkChoice::AlwaysSiteAllRepos, "GitHub.com", ws),
            Effect::Persist(vec![pat("github.com")], OpenUrlBinding::AllRepos)
        );
        assert_eq!(
            effect(LinkChoice::AlwaysSiteThisRepo, "GitHub.com", ws),
            Effect::Persist(
                vec![pat("github.com")],
                OpenUrlBinding::Repo(ws.to_path_buf())
            )
        );
        // "Any site" is this repo only; any site for every repo is CLI-only.
        assert_eq!(
            effect(LinkChoice::AlwaysAnyThisRepo, "github.com", ws),
            Effect::Persist(
                vec![HostPattern::Any],
                OpenUrlBinding::Repo(ws.to_path_buf())
            )
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
                "*.github.com".into(),
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
        assert_eq!(g.always, [pat("*.github.com"), pat("docs.rs")]);
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

    /// SPEC R-PERM.3.4, R5.3.1: a form's answer is read back only as an
    /// answer the question offered; anything else is a deny.
    #[test]
    fn a_form_value_is_read_only_as_an_offered_answer() {
        let offered = [LinkChoice::Deny, LinkChoice::Once];
        for choice in LinkChoice::ALL {
            let read = LinkChoice::from_value(choice.value(), &LinkChoice::ALL);
            assert_eq!(read, choice, "{} round-trips", choice.value());
        }
        assert_eq!(LinkChoice::from_value(" ONCE ", &offered), LinkChoice::Once);
        // An older build's values meant every repo; they are no answer now.
        for old in ["always-site", "always-any-site"] {
            assert_eq!(
                LinkChoice::from_value(old, &LinkChoice::ALL),
                LinkChoice::Deny,
                "{old}"
            );
        }
        for unread in ["", "yes", "always-any-site", "session", "🤷"] {
            assert_eq!(
                LinkChoice::from_value(unread, &offered),
                LinkChoice::Deny,
                "'{unread}' is not consent"
            );
        }
    }

    /// SPEC R-LINK.6, R-PERM.3.2: a link nobody could be asked about names
    /// the grant and says why it was not asked.
    #[test]
    fn an_unasked_refusal_says_why_and_names_the_grant() {
        let line = unasked_refusal_line(
            &t("https://github.com/o/r"),
            "the MCP client cannot show questions",
        );
        assert!(
            line.contains("could not ask: the MCP client cannot show questions."),
            "{line}"
        );
        assert!(
            line.contains("`ahma permissions grant open-url github.com`"),
            "{line}"
        );
        assert!(
            !line.contains("re-run this command and ahma asks"),
            "{line}"
        );
        assert!(line.ends_with("https://github.com/o/r"), "{line}");
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

    /// SPEC R-LINK.4: an address a name resolves to is local when it is this
    /// machine or the local network, in either family.
    #[test]
    fn a_resolved_address_is_local_when_it_is_this_machine_or_the_lan() {
        for local in [
            "127.0.0.1",
            "10.1.2.3",
            "169.254.169.254",
            "100.64.0.1",
            "::1",
            "::",
            "fd00::1",
            "fe80::1",
            "::ffff:192.168.1.1",
        ] {
            assert!(ip_is_local(local.parse().unwrap()), "{local}");
        }
        for public in ["140.82.112.3", "2606:4700::1111", "::ffff:8.8.8.8"] {
            assert!(!ip_is_local(public.parse().unwrap()), "{public}");
        }
    }
}
