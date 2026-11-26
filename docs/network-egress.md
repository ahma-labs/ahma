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
6.7+ — and it denies everything by default. A blocked request gets a `403`
(plain HTTP) or `407` (HTTPS) whose body names the host and the command that
allows it, `ahma network allow <host>`, so a tool that prints the proxy's answer,
or `curl -v`, says why it failed.

With an empty `[network] allow`, restricted mode used to break the first `cargo build`,
because cargo needs `index.crates.io`, `static.crates.io`, `crates.io` and `github.com`.
Profiles now supply those hosts, so a narrow, deliberate allowlist works out of the box.

## The mechanism: profiles already solved this for paths

A [sandbox profile](permissions.md) is a pre-answered bundle of grant questions.
`rust`, `node`, `go`, `python`, `ruby`, `android`, `apple`, `common`, `gh` and `sccache` each declare the filesystem paths their
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

### `python`

| Host | Why | Source |
|---|---|---|
| `pypi.org` | The package index: the simple API pip and uv resolve against, and the JSON API | verified 2026-11-26 against a live fetch |
| `files.pythonhosted.org` | Wheel and sdist downloads; every file URL in `pypi.org`'s index points here, and answers `200` with no further redirect | same |

The profile reads pyenv's interpreters (`~/.pyenv`), uv's managed interpreters and
tools (`~/.local/share/uv/python`, `…/uv/tools`) and pip's macOS cache. Not uv's
data directory as a whole: `uv auth` keeps credentials there.

### `ruby`

| Host | Why | Source |
|---|---|---|
| `rubygems.org` | Gem downloads (`/gems/<name>-<version>.gem`, answered directly) and the API `gem install` uses | verified 2026-11-26 against a live fetch |
| `index.rubygems.org` | The compact index Bundler resolves against | same |

The profile reads rbenv's rubies (`~/.rbenv`) and user-installed gems
(`~/.gem/ruby`, `~/.gem/specs`). Not `~/.gem` or `~/.bundle` as a whole:
`~/.gem/credentials` holds your RubyGems API key, and `~/.bundle/config` can hold
private gem-server passwords.

### `android`

| Host | Why |
|---|---|
| `dl.google.com` | Google's Maven repository (AndroidX, the Android Gradle plugin) and SDK component downloads |
| `maven.google.com` | The documented alias of Google's Maven repository; Gradle's `google()` resolves against it |
| `repo.maven.apache.org` | Maven Central: Gradle's `mavenCentral()` and Maven's default remote repository |
| `repo1.maven.org` | Maven Central's canonical host; older POMs and Maven settings name it directly |
| `services.gradle.org` | Gradle wrapper distributions (`gradlew` downloads the pinned Gradle version) |
| `plugins.gradle.org` | The Gradle plugin portal: `plugins {}` blocks resolve plugin ids and artifacts here |

### `apple`

| Host | Why |
|---|---|
| `github.com` | SwiftPM dependencies are git repositories, overwhelmingly hosted on GitHub |
| `cdn.cocoapods.org` | The CocoaPods spec CDN (`trunk` source) that `pod install` resolves against |

### `common`, `sccache`

Nothing. A shared cache directory is not an ecosystem and reaches nothing of its
own; every host belongs to the toolchain that fetches it. Inventing an entry here
would grant it to everyone who left `common` enabled, which is everyone.

### `gh`

| Host | Why | Source |
|---|---|---|
| `api.github.com` | The REST and GraphQL API every `gh` command calls | [docs.github.com](https://docs.github.com/en/rest) |
| `github.com` | Authentication and the git operations `gh` performs | same |

The profile also lets `gh` write its own cache (`~/.cache/gh`), without which
`gh run view --log-failed` fails inside the sandbox. Run-log downloads redirect to
GitHub's blob storage; add that host yourself if you restrict the network and need
logs.

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
* **`upload.pypi.org`** (for `python`) and publishing endpoints generally.
  Publishing a package is a decision to make explicitly, not a side effect of a
  build being able to fetch.
* **uv's interpreter downloads** (`uv python install` fetches GitHub release
  assets from `objects.githubusercontent.com`), for the same reason as the
  `rust` exclusion above.
* **Docker Hub and other container registries.** `docker pull` and
  `docker build` fetch from the daemon, which runs outside the sandbox, and a
  sandboxed command cannot reach the daemon's socket anyway. Only a registry
  client run as a command (`crane`, `skopeo`, `oras`) needs a registry host; add
  yours to `[network] allow`.

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
| `[network] local_ports` (default `[]`) | Linux only: local TCP ports a command may connect to directly (`[5432, 6379]`). Each is open to **every** host. See [Local services](#local-services). |
| `[sandbox] profiles` | Removing a profile removes its paths *and* its hosts. |

The reason `profile_hosts` and `deny_profile_hosts` exist separately from
`[sandbox] profiles` is that "I write Go and want `~/.go` granted, but this
machine must never reach the public module mirror" is a real posture, and
removing `go` from `[sandbox] profiles` would answer a different — usually wrong
— question.

```toml
[sandbox]
profiles = ["rust", "node", "go", "python", "ruby", "android", "apple", "common", "gh", "sccache"]

[network]
restrict = true
allow = ["artifacts.internal.example"]   # composes with the profile hosts
deny_profile_hosts = ["go"]              # keep ~/.go, drop proxy.golang.org
```

## Local services

A restricted command still needs services on this machine: a test database, a cache, a
docker-compose service, a test server on a random port.

* **macOS** allows every `localhost` port, and only `localhost`. The exception is Docker's
  TCP API (`localhost:2375` and `2376`), which answers without authentication and can start
  a container that writes anywhere, so it stays shut, like Docker's socket.
* **Linux** cannot express "localhost only": Landlock matches the port, never the address.
  So only the proxy's port is open, and you list any others:

  ```toml
  [network]
  restrict = true
  local_ports = [5432, 6379]   # Postgres, Redis
  ```

  A listed port is open to **every** host, not only this machine: a command could send
  data to a server anywhere that listens on it. ahma says so at startup. A test server on
  a random port (bind to port 0, then connect) cannot be listed, so such tests fail under
  restriction on Linux. That is why restriction stays opt-in there.

Either way, a local service that forwards to the internet (a local HTTP proxy, a browser's
remote-debugging port) is a way out that ahma cannot see.

## Git over SSH

SSH does not read `HTTP_PROXY`. So while restriction is on, ahma gives every command a
`GIT_SSH_COMMAND` whose `ProxyCommand` is `ahma ssh-proxy`: git's ssh connects through
the egress proxy, which decides as it does for HTTPS. `git push` to `git@github.com:…`
works when `github.com` is allowed (the `rust`, `gh` and `apple` profiles allow it), and an
unlisted host is asked about, or refused with the `ahma network allow <host>` command.

It replaces your own `GIT_SSH_COMMAND` and `core.sshCommand` for these commands.
`~/.ssh/config` still applies, keys and agents included, except its `ProxyCommand` and
`ProxyJump`: the command line's `ProxyCommand` wins. Plain `ssh`, which reads no such
variable, still cannot connect under restriction.

## The default stays opt-in, for now

`[network] restrict` defaults to `false`. Restricting by default is planned for macOS
(SPEC R-HARDEN.4) once it has been used day to day. On
Linux it stays opt-in: a test server on a random port cannot be reached under restriction
there ([Local services](#local-services)). Until then, turn it on yourself.

## Managing network grants and permissions

Allowed hosts can be managed from the command line, inspected in the permission ledger, or requested interactively by AI models via the `network_grant` MCP tool.

### CLI commands

```bash
ahma network allow crates.io        # add host to [network].allow
ahma network list                   # view current allowlist and restrictions
ahma network revoke crates.io       # remove host from [network].allow

ahma permissions list --kind net-host             # view network grants in ledger
ahma permissions revoke net-host crates.io --yes  # revoke via unified ledger
```

### The `network_grant` MCP tool

When an AI agent encounters a network block, it can invoke the `network_grant` tool:
- **One question**: `network_grant` with `confirm: true` asks the human at a prompt that shows the risk analysis and the exact line that would be added to `~/.ahma/settings.toml`; agents are told not to ask in chat first, since that prompt is the decision. Without `confirm: true` it only returns that preview and asks nobody.
- **Hard denylist**: Blanket `*`, `localhost`, and private/loopback/cloud-metadata IP addresses (`169.254.169.254`) are refused outright even with confirmation.
- **Human approval gate**: `confirm: true` never grants by itself, for any client. A client that declared MCP elicitation gets a real prompt; every other caller (the in-process agent, a headless harness, a client with no prompt) is told the `ahma network allow <host>` command the human must run — it is never assumed to have asked you first.
- **Immediate effect**: Human-approved grants take effect immediately for the active session and persist to `~/.ahma/settings.toml` across restarts.

## Listening for connections

ahma does not restrict where a sandboxed command listens. A dev server it starts
on every interface (`0.0.0.0`, the default for Next.js, Spring Boot, a Go `:8080`
and many others) can be reached by any device on the networks this machine is on:
a café's Wi-Fi, an office LAN. ahma says so at startup, in `status` and in the TUI,
and asks agents to bind `127.0.0.1` instead (most dev servers take
`--host 127.0.0.1`).

It is not a grant because no kernel ahma uses can express "loopback only". On
macOS, Seatbelt can refuse every TCP bind but cannot re-allow loopback, and its
inbound filter has no effect. Linux Landlock filters binds by port only, and
Windows has no filter. Refusing all listening would break every local test server.
SPEC R-LISTEN records the evidence.

## Limits you still have

* Plain `ssh` (not through `git`) cannot connect under restriction: it does not use the
  proxy, and only git reads the variable ahma sets ([Git over SSH](#git-over-ssh)).
* Interactive approval still applies: a subprocess reaching an unlisted host
  raises an MCP `elicitation/create` prompt when a capable client is attached
  (SPEC R-WEB.16.8), so profiles reduce the prompt volume rather than replacing
  the mechanism.
* A command run through a terminal hook (Claude Code's Bash, with `ahma setup`'s
  hooks) is restricted the same way, but an unlisted host is **refused, not
  asked**: a hook has no MCP client to ask. Allow it with
  `ahma network allow <host>`, which the refusal names.
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
