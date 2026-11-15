//! How a browser tab proves it belongs to the human who started `ahma web`
//! (SPEC R-GUI.2).
//!
//! The one thing a sandboxed command cannot do is read the controlling terminal,
//! so that is where the proof starts: a short, single-use pairing code is
//! written there and typed into the page. The page gets a token of its own and
//! keeps it in memory. Nothing secret is ever in a URL, an argument, a file, a
//! cookie or a log — the places a command in the sandbox can read.
//!
//! The code is 40 bits and the server counts guesses, so it is not the whole
//! defence but the part that matters: five misses retire it and mint another,
//! fifteen lock pairing until the human presses Enter at the terminal (or, for
//! a server running in the background, stop it). A code also expires
//! ([`CODE_TTL`]).
//!
//! `ahma web` also copies the first code to the clipboard, which other
//! programs can read. There, [`PairingPolicy::burn_on_reuse`] turns a second
//! use of a spent code — the human pasting a code someone else already used —
//! into a revocation of every tab, and the server stops (SPEC R-GUI.2).

use std::collections::HashMap;
use std::sync::Arc;
use subtle::ConstantTimeEq;

/// The alphabet of a pairing code: Crockford's base32, without the letters that
/// read as digits.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const CODE_LEN: usize = 8;
/// Wrong guesses before the code is retired and another minted.
pub const MISSES_PER_CODE: u32 = 5;
/// Wrong guesses before pairing locks until the human unlocks it.
pub const MISSES_BEFORE_LOCK: u32 = 15;
/// How long a tab's token outlives its event stream: long enough for a network
/// blip or a laptop's sleep, short enough that a tab that is gone stops being a
/// credential.
pub const RECONNECT_GRACE: std::time::Duration = std::time::Duration::from_secs(60);
/// Tabs that may be paired at once. Each holds a connection open, and a browser
/// allows about six per origin.
pub const MAX_TABS: usize = 4;
/// How long a code is good for once shown.
pub const CODE_TTL: std::time::Duration = std::time::Duration::from_secs(120);
/// Spent codes remembered to recognise a second use.
const SPENT_REMEMBERED: usize = 8;

/// The rules that differ with where the code was put.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairingPolicy {
    /// How long a code is good for.
    pub code_ttl: std::time::Duration,
    /// A spent code used again revokes every tab and refuses all pairing
    /// from then on: set where the code was somewhere another program could
    /// read it.
    pub burn_on_reuse: bool,
    /// No code is accepted at all: the one client's token was minted with
    /// [`Auth::mint_tab`] (`ahma desktop`).
    pub pairing_closed: bool,
}

impl Default for PairingPolicy {
    fn default() -> Self {
        Self {
            code_ttl: CODE_TTL,
            burn_on_reuse: false,
            pairing_closed: false,
        }
    }
}

/// Where a freshly minted code goes: the controlling terminal.
pub type Announce = Arc<dyn Fn(&str) + Send + Sync>;

/// Why pairing did not produce a token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairError {
    WrongCode,
    /// Too many misses: the human unlocks it at the terminal.
    Locked,
    TooManyTabs,
    /// The code was shown too long ago; a new one has been minted.
    Expired,
    /// A spent code was used again where reuse burns: every tab is revoked
    /// and pairing is closed for good.
    Reused,
    /// This server takes no codes (`ahma desktop`).
    Closed,
}

/// A paired tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TabId(pub u64);

struct Tab {
    secret: String,
    /// When its event stream closed, while it is closed.
    closed_at: Option<std::time::Instant>,
}

struct Inner {
    code: String,
    code_minted: std::time::Instant,
    /// Codes already used, newest last.
    spent: Vec<String>,
    /// Set by a reuse that burns: nothing pairs again.
    burned: bool,
    misses_on_code: u32,
    misses_total: u32,
    next_tab: u64,
    tabs: HashMap<u64, Tab>,
}

/// The pairing code, the tokens it has produced, and the rules between them.
pub struct Auth {
    inner: poison_safe::Mutex<Inner>,
    announce: Announce,
    grace: std::time::Duration,
    policy: PairingPolicy,
}

/// A std mutex with the poisoning dealt with in one place.
mod poison_safe {
    pub struct Mutex<T>(std::sync::Mutex<T>);
    impl<T> Mutex<T> {
        pub fn new(value: T) -> Self {
            Self(std::sync::Mutex::new(value))
        }
        pub fn lock(&self) -> std::sync::MutexGuard<'_, T> {
            // A panic while holding this would leave a half-applied change to a
            // credential: carry on with what is there rather than die.
            self.0.lock().unwrap_or_else(|e| e.into_inner())
        }
    }
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Neither the code nor any token, ever.
        f.write_str("Auth(<redacted>)")
    }
}

/// 32 random bits per call, from the operating system's source, via `uuid`.
fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    let mut filled = 0;
    while filled < N {
        let bytes = *uuid::Uuid::new_v4().as_bytes();
        let take = (N - filled).min(bytes.len());
        out[filled..filled + take].copy_from_slice(&bytes[..take]);
        filled += take;
    }
    out
}

fn mint_code() -> String {
    let bytes: [u8; CODE_LEN] = random_bytes();
    bytes
        .iter()
        .map(|b| ALPHABET[(*b & 0x1f) as usize] as char)
        .collect()
}

/// How a code is shown: two groups of four.
pub fn display_code(code: &str) -> String {
    format!("{}-{}", &code[..CODE_LEN / 2], &code[CODE_LEN / 2..])
}

/// What a human typed, read as the code it means: case and separators do not
/// matter, and the letters that look like digits are the digits.
pub fn normalize(typed: &str) -> String {
    typed
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            other => other,
        })
        .collect()
}

impl Auth {
    /// A new pairing state, announcing its first code at once.
    pub fn new(announce: Announce) -> Self {
        let code = mint_code();
        announce(&display_code(&code));
        Self {
            inner: poison_safe::Mutex::new(Inner {
                code,
                code_minted: std::time::Instant::now(),
                spent: Vec::new(),
                burned: false,
                misses_on_code: 0,
                misses_total: 0,
                next_tab: 1,
                tabs: HashMap::new(),
            }),
            announce,
            grace: RECONNECT_GRACE,
            policy: PairingPolicy::default(),
        }
    }

    /// The same, under `policy`.
    pub fn with_policy(mut self, policy: PairingPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Replace the code with a fresh one and announce it.
    fn renew(&self, inner: &mut Inner) {
        inner.code = mint_code();
        inner.code_minted = std::time::Instant::now();
        inner.misses_on_code = 0;
        (self.announce)(&display_code(&inner.code));
    }

    /// Forget tabs whose stream has been closed longer than the grace.
    fn sweep(&self, inner: &mut Inner) {
        let grace = self.grace;
        inner
            .tabs
            .retain(|_, tab| tab.closed_at.is_none_or(|at| at.elapsed() < grace));
    }

    /// Try a typed code. On success the code is spent, a new one is announced
    /// for the next tab, and the token is `<tab>.<secret>`.
    pub fn pair(&self, typed: &str) -> Result<(TabId, String), PairError> {
        if self.policy.pairing_closed {
            return Err(PairError::Closed);
        }
        let mut inner = self.inner.lock();
        self.sweep(&mut inner);
        if inner.burned {
            return Err(PairError::Reused);
        }
        if inner.misses_total >= MISSES_BEFORE_LOCK {
            return Err(PairError::Locked);
        }
        let typed = normalize(typed);
        let right =
            typed.len() == CODE_LEN && bool::from(typed.as_bytes().ct_eq(inner.code.as_bytes()));
        if self.policy.burn_on_reuse
            && !right
            && inner
                .spent
                .iter()
                .any(|spent| bool::from(spent.as_bytes().ct_eq(typed.as_bytes())))
        {
            tracing::warn!(
                "gui: a spent pairing code was used again; revoking every tab and closing pairing"
            );
            inner.burned = true;
            inner.tabs.clear();
            return Err(PairError::Reused);
        }
        if inner.code_minted.elapsed() >= self.policy.code_ttl {
            self.renew(&mut inner);
            return Err(PairError::Expired);
        }
        if !right {
            inner.misses_on_code += 1;
            inner.misses_total += 1;
            if inner.misses_total >= MISSES_BEFORE_LOCK {
                tracing::warn!(
                    "gui: pairing is locked after repeated wrong codes; Enter at the terminal unlocks it"
                );
                return Err(PairError::Locked);
            }
            if inner.misses_on_code >= MISSES_PER_CODE {
                self.renew(&mut inner);
            }
            return Err(PairError::WrongCode);
        }
        if inner.tabs.len() >= MAX_TABS {
            // The code is not spent: nothing was paired.
            return Err(PairError::TooManyTabs);
        }
        let (tab, token) = Self::new_tab(&mut inner);
        // Single use.
        let spent = std::mem::take(&mut inner.code);
        inner.spent.push(spent);
        if inner.spent.len() > SPENT_REMEMBERED {
            inner.spent.remove(0);
        }
        inner.misses_total = 0;
        self.renew(&mut inner);
        Ok((tab, token))
    }

    /// A tab and its `<tab>.<secret>` token.
    fn new_tab(inner: &mut Inner) -> (TabId, String) {
        let id = inner.next_tab;
        inner.next_tab += 1;
        let secret = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        inner.tabs.insert(
            id,
            Tab {
                secret: secret.clone(),
                closed_at: None,
            },
        );
        (TabId(id), format!("{id}.{secret}"))
    }

    /// A tab for a client this process trusts already (`ahma desktop`'s own
    /// window), without a code.
    pub fn mint_tab(&self) -> (TabId, String) {
        Self::new_tab(&mut self.inner.lock())
    }

    /// Which tab a bearer token belongs to, compared in constant time.
    pub fn authenticate(&self, token: &str) -> Option<TabId> {
        let (id, secret) = token.split_once('.')?;
        let id: u64 = id.parse().ok()?;
        let mut inner = self.inner.lock();
        self.sweep(&mut inner);
        let tab = inner.tabs.get(&id)?;
        bool::from(tab.secret.as_bytes().ct_eq(secret.as_bytes())).then_some(TabId(id))
    }

    /// A tab's event stream opened: its token is good for as long as it is.
    pub fn stream_opened(&self, tab: TabId) {
        if let Some(t) = self.inner.lock().tabs.get_mut(&tab.0) {
            t.closed_at = None;
        }
    }

    /// A tab's event stream closed. Its token keeps working for the reconnect
    /// grace, so a dropped connection does not mean pairing again, and then
    /// stops.
    pub fn stream_closed(&self, tab: TabId) {
        if let Some(t) = self.inner.lock().tabs.get_mut(&tab.0) {
            t.closed_at = Some(std::time::Instant::now());
        }
    }

    /// A tab left for good.
    pub fn remove(&self, tab: TabId) {
        self.inner.lock().tabs.remove(&tab.0);
    }

    /// The human pressed Enter at the terminal: forgive the misses and show a
    /// code again, a new one if the last was retired.
    pub fn unlock(&self) {
        let mut inner = self.inner.lock();
        if inner.misses_total > 0 || inner.code_minted.elapsed() >= self.policy.code_ttl {
            inner.misses_total = 0;
            self.renew(&mut inner);
        } else {
            (self.announce)(&display_code(&inner.code));
        }
    }

    /// Tabs paired right now.
    pub fn tabs(&self) -> usize {
        self.inner.lock().tabs.len()
    }

    #[cfg(test)]
    fn with_code_ttl(mut self, ttl: std::time::Duration) -> Self {
        self.policy.code_ttl = ttl;
        self
    }

    #[cfg(test)]
    fn with_grace(mut self, grace: std::time::Duration) -> Self {
        self.grace = grace;
        self
    }

    #[cfg(test)]
    fn current_code(&self) -> String {
        self.inner.lock().code.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// An `Auth` whose announcements are captured instead of printed.
    fn auth() -> (Auth, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let auth = Auth::new(Arc::new(move |code: &str| {
            sink.lock().unwrap().push(code.to_string())
        }));
        (auth, seen)
    }

    #[test]
    fn a_code_is_eight_characters_from_the_alphabet_and_shown_in_two_groups() {
        let (auth, seen) = auth();
        let code = auth.current_code();
        assert_eq!(code.len(), CODE_LEN);
        assert!(code.bytes().all(|b| ALPHABET.contains(&b)), "{code}");
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            [display_code(&code)],
            "announced once, at the start"
        );
        assert_eq!(display_code("ABCD1234"), "ABCD-1234");
    }

    /// What a human types is read generously: case, spaces, the dash, and the
    /// letters that look like digits.
    #[test]
    fn a_typed_code_is_read_the_way_it_was_meant() {
        assert_eq!(normalize(" abcd-ef12 "), "ABCDEF12");
        assert_eq!(normalize("O0Il1L"), "001111");
        let (auth, _) = auth();
        let code = auth.current_code();
        let typed = format!("{}-{}", &code[..4].to_lowercase(), &code[4..]);
        assert!(auth.pair(&typed).is_ok());
    }

    /// A code works once: pairing spends it and announces another, so a code
    /// seen on a screen or in scrollback is worth nothing afterwards.
    #[test]
    fn a_code_is_single_use() {
        let (auth, seen) = auth();
        let code = auth.current_code();
        let (tab, token) = auth.pair(&code).expect("the right code pairs");
        assert_eq!(auth.authenticate(&token), Some(tab));
        assert_eq!(auth.pair(&code), Err(PairError::WrongCode));
        assert_eq!(seen.lock().unwrap().len(), 2, "a fresh code was announced");
        assert_ne!(auth.current_code(), code);
    }

    /// Five misses retire the code, even for the human who then types it right.
    #[test]
    fn repeated_misses_retire_the_code() {
        let (auth, seen) = auth();
        let code = auth.current_code();
        for _ in 0..MISSES_PER_CODE {
            assert_eq!(auth.pair("WRONGCODE"), Err(PairError::WrongCode));
        }
        assert_ne!(auth.current_code(), code, "a new code was minted");
        assert_eq!(seen.lock().unwrap().len(), 2, "and announced");
        assert_eq!(auth.pair(&code), Err(PairError::WrongCode));
    }

    /// Fifteen misses lock pairing — even the right code — until the human
    /// presses Enter at the terminal, which is the one place a command in the
    /// sandbox cannot reach.
    #[test]
    fn many_misses_lock_pairing_until_the_human_unlocks_it() {
        let (auth, _) = auth();
        let mut last = Err(PairError::WrongCode);
        for _ in 0..MISSES_BEFORE_LOCK {
            last = auth.pair("WRONGCODE").map(|_| ());
        }
        assert_eq!(last, Err(PairError::Locked));
        let code = auth.current_code();
        assert_eq!(
            auth.pair(&code),
            Err(PairError::Locked),
            "even the right one"
        );
        auth.unlock();
        let code = auth.current_code();
        assert!(auth.pair(&code).is_ok(), "pairing works again");
    }

    /// A token is for its own tab: another tab's secret, a forged id, a missing
    /// dot or a removed tab authenticate nothing.
    #[test]
    fn a_token_authenticates_only_its_own_tab() {
        let (auth, _) = auth();
        let (a, token_a) = auth.pair(&auth.current_code()).unwrap();
        let (b, token_b) = auth.pair(&auth.current_code()).unwrap();
        assert_ne!(a, b);
        assert_eq!(auth.authenticate(&token_a), Some(a));
        assert_eq!(auth.authenticate(&token_b), Some(b));
        let secret_a = token_a.split_once('.').unwrap().1;
        assert_eq!(auth.authenticate(&format!("{}.{}", b.0, secret_a)), None);
        // The failing case is named by its position: a secret is never printed.
        for (case, bad) in ["", ".", "x.y", "99.abcdef", &format!("{}.", a.0), secret_a]
            .into_iter()
            .enumerate()
        {
            assert_eq!(auth.authenticate(bad), None, "bad token case {case}");
        }
        auth.remove(a);
        assert_eq!(auth.authenticate(&token_a), None);
        assert_eq!(auth.authenticate(&token_b), Some(b));
    }

    /// Each tab holds a connection open and a browser allows about six, so the
    /// number that may pair is bounded; a refusal spends no code.
    #[test]
    fn the_number_of_paired_tabs_is_capped() {
        let (auth, _) = auth();
        for _ in 0..MAX_TABS {
            auth.pair(&auth.current_code()).unwrap();
        }
        assert_eq!(auth.tabs(), MAX_TABS);
        let code = auth.current_code();
        assert_eq!(auth.pair(&code), Err(PairError::TooManyTabs));
        assert_eq!(auth.current_code(), code, "the code was not spent");
    }

    /// Neither the code nor a token is ever in a log or a debug line.
    #[test]
    fn nothing_secret_is_in_debug() {
        let (auth, _) = auth();
        let code = auth.current_code();
        let (_, token) = auth.pair(&code).unwrap();
        let shown = format!("{auth:?}");
        assert!(!shown.contains(&auth.current_code()) && !shown.contains(&token));
    }

    /// A dropped connection is not a reason to pair again: the token outlives
    /// the stream for the grace, and not for ever, so a tab that is gone stops
    /// being a credential. A reconnect within the grace makes it good again.
    #[test]
    fn a_token_outlives_its_stream_by_the_grace_and_no_longer() {
        let (auth, _) = auth();
        let auth = auth.with_grace(std::time::Duration::from_millis(60));
        let (tab, token) = auth.pair(&auth.current_code()).unwrap();
        auth.stream_opened(tab);
        auth.stream_closed(tab);
        assert_eq!(auth.authenticate(&token), Some(tab), "within the grace");
        // It reconnects: the clock stops.
        auth.stream_opened(tab);
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(
            auth.authenticate(&token),
            Some(tab),
            "open streams do not expire"
        );
        // It goes, and is not back in time.
        auth.stream_closed(tab);
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(auth.authenticate(&token), None, "after the grace");
        assert_eq!(auth.tabs(), 0, "and it no longer counts against the cap");
    }

    /// A code is good for a short while only: a copy left in a clipboard
    /// history, a scrollback or a screenshot is worth nothing after it.
    #[test]
    fn a_code_expires_and_a_new_one_is_announced() {
        let (auth, seen) = auth();
        let auth = auth.with_code_ttl(std::time::Duration::from_millis(40));
        let code = auth.current_code();
        std::thread::sleep(std::time::Duration::from_millis(80));
        assert_eq!(auth.pair(&code), Err(PairError::Expired));
        assert_ne!(auth.current_code(), code, "a new code was minted");
        assert_eq!(seen.lock().unwrap().len(), 2, "and announced");
        assert!(auth.pair(&auth.current_code()).is_ok(), "the new one works");
    }

    /// Where the code was put somewhere another program could read it (the
    /// clipboard), a second use of a spent code means someone else got there
    /// first: every tab is revoked, and the caller stops the server.
    #[test]
    fn a_spent_code_used_again_revokes_every_tab_when_reuse_burns() {
        let (auth, _) = auth();
        let auth = auth.with_policy(PairingPolicy {
            burn_on_reuse: true,
            ..PairingPolicy::default()
        });
        let code = auth.current_code();
        let (tab, token) = auth.pair(&code).expect("the first use pairs");
        assert_eq!(auth.authenticate(&token), Some(tab));
        assert_eq!(auth.pair(&code), Err(PairError::Reused));
        assert_eq!(
            auth.authenticate(&token),
            None,
            "the first pairing is revoked"
        );
        assert_eq!(auth.tabs(), 0);
        assert_eq!(
            auth.pair(&auth.current_code()),
            Err(PairError::Reused),
            "and nothing pairs again: the server is stopping"
        );
    }

    /// Without that policy (the code was only ever on the terminal) a spent
    /// code is simply wrong.
    #[test]
    fn a_spent_code_is_just_wrong_by_default() {
        let (auth, _) = auth();
        let code = auth.current_code();
        let (tab, token) = auth.pair(&code).unwrap();
        assert_eq!(auth.pair(&code), Err(PairError::WrongCode));
        assert_eq!(auth.authenticate(&token), Some(tab));
    }

    /// `ahma desktop`: the window is the only client, its token is minted
    /// before it opens, and no code is ever shown or accepted.
    #[test]
    fn a_closed_pairing_mints_one_token_and_takes_no_code() {
        let (auth, seen) = auth();
        let auth = auth.with_policy(PairingPolicy {
            pairing_closed: true,
            ..PairingPolicy::default()
        });
        let (tab, token) = auth.mint_tab();
        assert_eq!(auth.authenticate(&token), Some(tab));
        let code = auth.current_code();
        assert_eq!(
            auth.pair(&code),
            Err(PairError::Closed),
            "not even the right code"
        );
        assert_eq!(seen.lock().unwrap().len(), 1, "nothing new announced");
    }

    #[test]
    fn a_token_is_sixty_four_hex_characters_after_the_tab() {
        let (auth, _) = auth();
        let (_, token) = auth.pair(&auth.current_code()).unwrap();
        let secret = token.split_once('.').unwrap().1;
        assert_eq!(secret.len(), 64);
        assert!(secret.bytes().all(|b| b.is_ascii_hexdigit()));
    }
}
