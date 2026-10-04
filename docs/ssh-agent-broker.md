# SSH key broker — `git push` over SSH from inside the sandbox

**Status:** Experimental (v0.22.3). Terminal hooks (Claude Code's Bash tool and other hooked
shells) only so far; commands run through MCP `run_terminal_command` still use your own
agent socket directly. SPEC: [R-CRED](../SPEC.md).

## Why

A sandboxed command may never read a private key, and no grant can change that. What `git
push` needs is to *use* the key: to sign one login to one server. Before the broker, that
worked only if your key happened to be loaded in your SSH agent, and on macOS the agent
starts empty at every login while the ssh you type reads the key file directly, so the
first sandboxed push failed with `Permission denied (publickey)`. The usual workaround was
to rewrite remotes to HTTPS.

Forwarding the agent is also all or nothing: any sandboxed process may sign for any server
with any key the agent holds, silently.

The broker replaces both. Each command gets an SSH agent of its own, served by ahma outside
the sandbox. It signs only for a server the connection proves it is talking to, only with a
human's yes for that key, that server and that workspace, and it records every signature.
The key never enters the sandbox.

## What happens on `git push`

1. The hooked command's `SSH_AUTH_SOCK` is the broker's socket. ssh asks it to sign the
   login to `github.com`.
2. ahma checks which server the connection is bound to — OpenSSH proves it with the
   server's own signature (`session-bind@openssh.com`) — and names it from `known_hosts`.
3. If you already allowed this key for this server in this workspace, it signs: through
   your own agent if the agent holds the key, otherwise with the key file, read on the host.
4. If not, it refuses, and the command's output gets one line:

   ```text
   Blocked until a human approves: ahma's SSH key broker did not sign for github.com with
   key SHA256:… (you@laptop) in this workspace. In Claude Code, re-run this command and
   ahma asks first; elsewhere a human runs `ahma permissions grant ssh-sign "SHA256:… for
   host:SHA256:…"` (add `--for 24h` for a lease), then re-run.
   ```

5. In Claude Code, re-running that command (and only that command) brings up Claude Code's
   own permission dialog first:
   *ahma: when this command last ran it asked to use your SSH key SHA256:… for github.com …
   Approve to let commands in this workspace sign with it for github.com for this
   session.* A yes records a session grant (it ends with that Claude Code session, and
   after 12 hours at most) and the retried push runs and signs. Other commands are never
   held for the question.

## Quickstart

Nothing to set up for an unencrypted ed25519 key in `~/.ssh` (the `ssh-keygen` default).
Push; when the broker refuses, push again and answer the dialog that comes first (once
per session).

To allow a key for a server permanently, or for a while, from any terminal:

```bash
# The subject is printed in the refusal line; preview first, then --yes to write.
ahma permissions grant ssh-sign "SHA256:<key> for host:SHA256:<host key>"
ahma permissions grant ssh-sign "SHA256:<key> for host:SHA256:<host key>" --for 7d --yes
ahma permissions list --kind ssh-sign
ahma permissions revoke ssh-sign "SHA256:<key> for host:SHA256:<host key>" --yes
```

`ssh-keygen -Y sign` (signed commits, file signatures) is a separate destination,
`sshsig:<namespace>`, granted the same way.

## Keys the host does not sign with

Passphrase-protected, RSA, ECDSA and security keys (`sk-*`) are used through your own SSH
agent: load them once per login with `ssh-add <key>` (on macOS, `ssh-add
--apple-use-keychain <key>` keeps the passphrase in the keychain). The broker still asks
before the agent signs.

## What is refused, always

| Request | Why |
| --- | --- |
| A login on a connection that did not prove its server (OpenSSH before 8.9, some libraries) | ahma signs only for a server it can name |
| A server whose host key is in no `known_hosts` entry | connect once from your own terminal to add it |
| A signature over data whose purpose ahma cannot read | it signs logins and `ssh-keygen -Y` signatures only |
| A forwarded connection (`ssh -A` onward) | it would hand the key's use to another machine |
| Adding, removing or locking keys, smartcard requests, other extensions | the broker lists and signs, nothing else |
| A connection from a process ahma did not spawn | another session's command does not borrow this one's grants |

## Reference

| Setting / file | Meaning |
| --- | --- |
| `[[sandbox.ssh_sign]]` in `~/.ahma/settings.toml` | `always` and lease grants: `key`, `destination`, `workspace`, `granted_at`, optional `expires_at` |
| `<runtime dir>/ssh-sign/` | session grants, one file each, bound to the harness process |
| `<runtime dir>/agent/` | the brokers' sockets; a sandboxed command may connect, never create or remove |
| `~/.ahma/permissions-audit.jsonl` | every grant, kind `ssh-sign` |

## See also

- [Security sandbox](security-sandbox.md) — why key files are unreadable, and what is.
- [Permissions and grants](permissions.md) — tiers, the ledger, the harness dialog.
- [SPEC R-CRED](../SPEC.md) and R6.2.3.
