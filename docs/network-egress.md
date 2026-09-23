# Network egress: restricted mode, and the hosts profiles grant

How `--restrict-network` decides which hostnames a sandboxed subprocess may
reach, and why turning it on no longer breaks your first build.

Related: [security-sandbox.md](security-sandbox.md#network-egress) for the
platform enforcement story, [permissions.md](permissions.md) for the wider
grant model.

## The problem this fixes

`--restrict-network` / `[network] restrict = true` routes every sandboxed
subprocess through a guarded local proxy and forwards only hostnames on an
allowlist. It is genuinely kernel-enforced — Seatbelt denies outbound IP except
the proxy on macOS, Landlock restricts outbound TCP to the proxy port on Linux
6.7+ — and it denies everything by default.

Almost nobody turned it on, because with an empty `[network] allow` the first
command failed:

```
$ ahma serve --restrict-network
$ cargo build
    Updating crates.io index
error: failed to get `serde` as a dependency
  caused by: failed to fetch `https://index.crates.io/`
```

To get anywhere you had to already know that cargo resolves against
`index.crates.io`, downloads from `static.crates.io`, hits `crates.io` for the
API, and clones git dependencies from `github.com`. Then repeat that research for
npm and Go. That research *was* the adoption barrier, so the configuration
everyone actually ran was the unrestricted one.

That matters more than a usability complaint. The eighth escape in Pillar
Security's 2026 series landed on an eval sandbox whose single sanctioned egress
path — a package-registry proxy — turned out to carry a zero-day, and that one
trusted component was therefore the entire boundary. The lesson is not "never
allow egress". It is that the sanctioned path has to be **deliberate and
narrow**. A path nobody can configure is neither, because the configuration
people fall back to permits everything.

## The mechanism: profiles already solved this for paths

A [sandbox profile](permissions.md) is a pre-answered bundle of grant questions.
`rust`, `node`, `go` and `common` each declare the filesystem paths their
toolchain needs, ship enabled by default, and are refusable individually through
`[sandbox] profiles`.

Each profile now also declares the **hostnames** its toolchain must reach. When
restriction is on, the union of enabled profiles' hosts seeds the proxy's
allowlist, alongside your own `[network] allow`.

```
effective allowlist  =  [network] allow
                     ∪  hosts of every profile in [sandbox] profiles
                        (minus [network] deny_profile_hosts,
                         all of them if [network] profile_hosts = false)
```

They **compose**. Adding one internal host of your own does not switch the
toolchain hosts off, and the toolchain hosts do not suppress yours. Either alone
is a working configuration.

Source of truth is the profile data files (`ahma_mcp/profiles/*.toml`) — readable,
copyable, and diffable, which a compiled-in list is not.

## What the default install ships

Every entry below was verified against what the toolchain actually contacts, not
copied from a firewall blog post. The `reason` recorded next to each host in the
profile file is that verification, so a future reader can re-check it rather than
trust it.

### `rust`

| Host | Why | Source |
|---|---|---|
| `index.crates.io` | Sparse registry index — cargo's default dependency-resolution protocol since 1.70 | [crates.io data access](https://crates.io/data-access) |
| `static.crates.io` | `.crate` downloads | the `dl` field of `https://index.crates.io/config.json` |
| `crates.io` | Registry API (publish, yank, search, owner) | the `api` field of the same `config.json` |
| `static.rust-lang.org` | rustup channel manifests and toolchain/component downloads | [rustup book, other installation methods](https://rust-lang.github.io/rustup/installation/other.html) |
| `github.com` | Git dependencies over HTTPS, and the legacy git index `rust-lang/crates.io-index` | [Cargo book, git authentication](https://doc.rust-lang.org/cargo/appendix/git-authentication.html) |

### `node`

| Host | Why | Source |
|---|---|---|
| `registry.npmjs.org` | The npm registry — metadata *and* tarballs; `dist.tarball` URLs point back at the same host | verified against a live `registry.npmjs.org` package document |
| `registry.yarnpkg.com` | Yarn classic's registry alias; a `yarn.lock` resolves against this name | yarn's default registry |
| `nodejs.org` | Runtime tarballs for `nvm install` — the profile grants `~/.nvm`, so it should be usable | nvm's default `NVM_NODEJS_ORG_MIRROR` is `https://nodejs.org/dist` |

### `go`

| Host | Why | Source |
|---|---|---|
| `proxy.golang.org` | The default `GOPROXY` module mirror | [proxy.golang.org](https://proxy.golang.org/) |
| `sum.golang.org` | The default `GOSUMDB` checksum database — module authentication fails closed without it | same |

### `common`

Nothing. A shared cache directory is not an ecosystem and reaches nothing of its
own; every host belongs to the toolchain that fetches it. Inventing an entry here
would grant it to everyone who left `common` enabled, which is everyone.

### Deliberately *not* shipped

* **`storage.googleapis.com`** (for `go`). Widely repeated advice says
  `proxy.golang.org` redirects module downloads to a GCS bucket, so you must
  allowlist the storage host too. Re-verified against a real module fetch:
  `proxy.golang.org/<module>/@v/<version>.zip` answers `200` directly, with no
  cross-host redirect. Since host matching cannot express "one bucket", shipping
  it would have handed every sandboxed command reach into all of Google Cloud
  Storage — a general-purpose, world-readable, attacker-writable object store —
  to solve a problem that no longer exists.
* **`objects.githubusercontent.com`, `codeload.github.com`,
  `raw.githubusercontent.com`** (for `rust`). These serve release assets and
  archive tarballs. Cargo's git fetches and crate downloads do not use them; a
  `build.rs` that downloads a prebuilt binary from a GitHub release does, and
  that is exactly the kind of thing that should be an explicit decision.
* **`index.golang.org`** — a feed of new module versions, consumed by tooling
  that watches the ecosystem, never by a build.
* **PyPI, RubyGems, Maven Central, Docker Hub, …** — there is no shipped profile
  for those toolchains, so there is nothing to hang the hosts on. Add them to
  `[network] allow` yourself, or contribute a profile.

If you need one of these, add it to `[network] allow`; it composes with
everything above.

## Host matching semantics

One matcher (`ahma_mcp::egress::host_pattern::HostPattern`) decides every
question, so there is no second implementation to drift. Patterns and candidate
hostnames are both normalised first: ASCII-lowercased, with a single trailing
root dot removed.

| Pattern | Matches | Does not match |
|---|---|---|
| `crates.io` | `crates.io`, `CRATES.IO`, `crates.io.` | `index.crates.io`, `evilcrates.io`, `crates.io.evil.example` |
| `*.crates.io` | `index.crates.io`, `static.crates.io` | `crates.io`, `a.b.crates.io`, `evilcrates.io` |
| `*` | everything | — |

Four rules, each with a regression test:

1. **A wildcard covers exactly one label.** `*.crates.io` does not reach
   `a.b.crates.io`. Depth is where a subdomain takeover on some forgotten
   third-level name becomes egress, so widening it must be something someone
   writes down.
2. **A wildcard does not cover its own base.** List both if you mean both.
3. **Matching is label-boundary-anchored, never a suffix test.** The classic bug
   in this code is `host.ends_with(suffix)`, which lets `evilcrates.io` satisfy a
   rule written for `crates.io` — an attacker registers the concatenation and the
   allowlist hands them the traffic.
4. **Non-ASCII is rejected, not transformed.** A pattern with non-ASCII bytes
   fails to parse; a candidate with them matches nothing, not even `*`. ahma does
   not do IDNA/UTS-46 conversion here, for the reason homograph attacks work:
   `сrates.io` (Cyrillic `с`) and `crates.io` render identically but are
   different hosts, and a matcher that silently folded one into the other would
   be deciding on your behalf that they are the same. Internationalised domains
   are still reachable — write the punycode A-label (`xn--80ak6aa92e.com`) that
   DNS actually carries.

A malformed entry (`https://crates.io`, `crates.io:443`, `a*.example.com`) is
**dropped with a warning**, never coerced. An entry that can never match must not
look like one that can, or you read your own settings file, see your domain, and
conclude egress works.

`*` is accepted in `[network] allow` — it is your machine — but **refused inside a
profile**. Profiles are enabled by default, so a `*` in one would be a default-on
blanket egress grant arriving through a data file: precisely the invisible,
unrefusable grant the profile system exists to eliminate.

## Disclosure: which hosts, and who granted each

A merged, anonymous list of reachable hosts is not refusable. Someone who sees
`proxy.golang.org` and writes no Go cannot tell whether removing it is safe.
Naming the granting profile turns that into "do I want the `go` profile?", which
they can answer. This is the host half of SPEC R-PERM.5.2.

**`ahma permissions list`** shows each profile's hosts underneath that profile's
path grants, with the reason, and states whether they are currently in effect:

```
sandbox profiles (built-in toolchain carve-outs):
  • rust — Rust toolchain: cargo registry/git caches, rustup toolchains
      /Users/you/.cargo  (read+execute)
      /Users/you/.cargo/registry  (read+write)
      …
      network hosts (not in effect — egress is unrestricted; these apply under --restrict-network):
        index.crates.io  — sparse registry index — cargo's default dependency-resolution protocol since 1.70
        static.crates.io  — crate .crate downloads; the `dl` endpoint named by https://index.crates.io/config.json
        …

subprocess network egress: UNRESTRICTED (all hosts reachable).
  Turn on `--restrict-network` / `[network] restrict` to route subprocesses through
  the guarded proxy. The profiles above would then make these reachable, and
  nothing else:
  index.crates.io       builtin-profile(rust) — sparse registry index …
  registry.npmjs.org    builtin-profile(node) — the npm registry …
  proxy.golang.org      builtin-profile(go)   — the default GOPROXY module mirror
```

**Server startup**, when restriction is on, logs the same union with the same
attribution, plus how to switch the profile-contributed half off.

## Operator controls

| Setting | Effect |
|---|---|
| `[network] restrict` (or `--restrict-network`) | Master switch. **Off by default.** |
| `[network] allow` | Your own hosts. Always in effect; composes with profile hosts. |
| `[network] profile_hosts` (default `true`) | Let enabled profiles contribute hosts. `false` drops **all** profile hosts and keeps **every** profile's path grants. |
| `[network] deny_profile_hosts` (default `[]`) | The same, one profile at a time, by name. |
| `[sandbox] profiles` | Removing a profile removes its paths *and* its hosts. |

The reason `profile_hosts` and `deny_profile_hosts` exist separately from
`[sandbox] profiles` is that "I write Go and want `~/.go` granted, but this
machine must never reach the public module mirror" is a real posture, and
removing `go` from `[sandbox] profiles` would answer a different — usually wrong
— question.

```toml
[sandbox]
profiles = ["rust", "node", "go", "common"]

[network]
restrict = true
allow = ["artifacts.internal.example"]   # composes with the profile hosts
deny_profile_hosts = ["go"]              # keep ~/.go, drop proxy.golang.org
```

## The default stays opt-in — on purpose

`[network] restrict` still defaults to `false`, and this work does not change
that.

That is a deliberate decision, not an unfinished job. Profiles make restricted
mode survive a first `cargo build`, `npm install` or `go mod download`, but
flipping the default on today would still break the first command for everyone
whose toolchain has no shipped profile — which is most toolchains. Making
restricted mode painless enough that defaulting it on becomes a responsible
decision is the *point* of this change; making that decision is a separate one,
to be taken on evidence.

The same note is in the code, in
`ahma_mcp::egress::host_grants` and on
`ahma_common::config::NetworkSettings::restrict`, so a later reader does not
"finish the job" by flipping it.

## Limits you still have

* Interactive approval still applies: a subprocess reaching an unlisted host
  raises an MCP `elicitation/create` prompt when a capable client is attached
  (SPEC R-WEB.16.8), so profiles reduce the prompt volume rather than replacing
  the mechanism.
* Allowlisting is by **hostname**. The proxy re-resolves the host and refuses
  private/loopback/link-local/cloud-metadata addresses, so an allowed name cannot
  DNS-rebind to `127.0.0.1` — but it cannot distinguish two tenants of one
  hostname. That is why `storage.googleapis.com` is not shipped.
* Enforcement is kernel-level on macOS (Seatbelt) and Linux 6.7+ (Landlock), and
  **advisory elsewhere** — a tool that ignores `HTTP_PROXY` or opens a raw socket
  is not contained on Windows or older Linux kernels. ahma says which applies at
  startup rather than implying uniform enforcement.
* A profile host is reachable by *every* sandboxed command, not only by the
  toolchain that asked for it. Host allowlisting has no per-process dimension.
