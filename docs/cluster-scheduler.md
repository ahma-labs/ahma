# Local Cluster Scheduler

> **Experimental** — introduced in v0.7. mDNS peer discovery is a stub; static peer configuration is functional.

The cluster scheduler routes decompose sub-tasks to `ahma worker` peers on your local network or Tailscale mesh. Each peer runs its own local LLM (e.g. Ollama with `gemma4`) and its own kernel sandbox. The coordinator picks the least-loaded peer that has the requested model available.

## Why a local cluster?

A single-machine Ollama instance can only serve a few concurrent requests before latency degrades. If you have multiple machines (a laptop, a workstation, a home server, a company build machine), the cluster scheduler spreads sub-tasks across them — all running local models, all keeping data on your infrastructure.

This enables parallel AI workloads that would otherwise be throttled by a single machine's GPU without sending a single token to a cloud provider.

## Architecture

```
DecomposeOrchestrator (coordinator)
  │  for each sub-task
  ▼
ClusterScheduler
  │  1. filter peers with model available
  │  2. pick lowest active_ops (least-loaded)
  │  3. POST signed TaskManifest to peer HTTP bridge
  ▼
ahma worker peer (own machine or LAN/Tailscale)
  │  verify signature
  │  run sub-task inside kernel sandbox
  ▼
TaskResult returned to coordinator
```

## Configuring static peers

Create `~/.ahma/cluster/peers.json`:

```json
[
    {
        "id": "workstation",
        "addr": "http://192.168.1.10:3000",
        "models": ["gemma4", "llama3.2:3b"],
        "active_ops": 0,
        "reachable": true
    },
    {
        "id": "homeserver",
        "addr": "http://homeserver.tail12345.ts.net:3000",
        "models": ["gemma4"],
        "active_ops": 0,
        "reachable": true
    }
]
```

Peers must be running `ahma serve http` with a shared cluster key (see below).

## Starting a worker peer

On each peer machine:

```bash
# Start ahma HTTP bridge on port 3000
ahma serve http --port 3000
```

The peer must be reachable at the `addr` configured in `peers.json`.

## Security: signed task manifests

The coordinator signs each task manifest with HMAC-SHA256 before sending it to a peer.
The peer verifies the signature in constant time before executing. This prevents an
untrusted machine on the same network from injecting or replaying tasks.

**Threat model:**

| Threat | Mitigation |
|--------|------------|
| Task injection | HMAC-SHA256 signature over `task_id\|prompt\|model\|llm_base_url\|nonce\|issued_at` |
| Replay attack | `issued_at` timestamp checked; manifests older than 60 s are rejected |
| Timing oracle | Signature comparison uses `subtle::ConstantTimeEq` |
| Key brute-force | 256-bit random key; rotate with `ahma cluster rotate-key` |

To set the shared key, write a 32+ byte random value to a file and reference it:

```bash
# Generate a strong key once per cluster (run on the coordinator)
openssl rand -hex 32 > ~/.ahma/cluster/shared.key
chmod 600 ~/.ahma/cluster/shared.key
```

Each peer must have the same key file at the same path.

## Peer discovery

| Method | Status |
|--------|--------|
| Static `~/.ahma/cluster/peers.json` | Working |
| mDNS (`_ahma-worker._tcp.local`) | Working — zero-config LAN discovery via the `mdns-sd` crate |
| Tailscale | Configure static peers using Tailscale hostnames |

## Scheduler behaviour

- Peers without the requested model are excluded.
- Among eligible peers, the one with the lowest `active_ops` is selected. If VRAM free < 1 GiB an additional penalty is applied.
- If no peer is available, the orchestrator falls back to running the sub-task locally.
- Peers announce their capabilities (loaded models, VRAM, concurrency) via heartbeat; the coordinator updates its registry on each heartbeat.

## Using via ahma_core

```rust
use ahma_core::{WorkerRegistry, ClusterScheduler, TaskManifest};

let registry = WorkerRegistry::new(60);  // 60s TTL
registry.load_static_peers()?;

// Key is 32+ raw bytes; load from file, not hard-coded
let key = std::fs::read("~/.ahma/cluster/shared.key")?;
let scheduler = ClusterScheduler::new(registry, key);

let manifest = TaskManifest {
    task_id: "sub_1".into(),
    prompt: "What are the key risks?".into(),
    model: "gemma4".into(),
    llm_base_url: "http://localhost:11434/v1".into(),
    max_tokens: 256,
    timeout_secs: 30,
    // nonce and issued_at are filled in by manifest.sign(&key)
    nonce: String::new(),
    issued_at: 0,
    signature: String::new(),
};

if let Some(result) = scheduler.schedule(manifest).await {
    println!("Answer from {}: {}", result.worker_id, result.text);
}
```

## Security — HMAC signing & replay protection

All cluster communication between nodes is authenticated with **HMAC-SHA256**.

### Signed task manifests

Every `TaskManifest` is signed before dispatch and verified on receipt. The fields `nonce`, `issued_at`, and `signature` are handled automatically by `TaskManifest::sign`:

```rust
manifest.sign(&key);      // fills nonce (UUID v4), issued_at (Unix seconds), signature
manifest.verify(&key)?;   // returns Err if signature is wrong or manifest has expired
```

`verify` rejects manifests that are **older than 60 seconds** (configurable via `valid_window_secs`).

### Replay protection (NonceCache)

The receiving node passes the manifest through `verify_with_nonce_cache`, which calls `verify` *and* records the nonce in an in-memory `NonceCache`:

```rust
// Receiving side:
if !manifest.verify_with_nonce_cache(&key, &scheduler.nonce_cache)? {
    // nonce was already seen — replay attack blocked
    bail!("Duplicate nonce rejected");
}
```

`NonceCache` automatically evicts entries older than `issued_at + valid_window_secs` to bound memory usage.

### Signed heartbeats

Peer heartbeats are also signed. The capabilities payload is serialised to JSON and signed with the shared key:

```rust
// Sending side (worker):
let payload = serde_json::to_string(&capabilities)?;
let sig = hmac_sha256_hex(&key, &payload);

// Receiving side (coordinator):
registry.receive_signed_heartbeat(peer_id, capabilities, &sig, &key)?;
```

The receiving side uses `subtle::ConstantTimeEq` for the signature comparison to prevent timing side-channels.

### Key management

- Store the shared key in `~/.ahma/cluster/shared.key` (32+ random bytes, `chmod 600`).
- The key fingerprint (first 8 hex chars of its SHA-256) is logged at startup for cross-node audit.
- Configure the path via `cluster.key_file` in `~/.ahma/config.toml`.

## See also

- [docs/decompose.md](decompose.md) — sub-tasks that the scheduler dispatches
- [docs/task-vault.md](task-vault.md) — vault contents that travel with the task
- [SPEC.md](../SPEC.md) — cluster scheduler design notes
