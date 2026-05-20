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

The coordinator signs each task manifest with a shared HMAC key before sending it to a peer. The peer verifies the signature before executing. This prevents an untrusted machine on the same network from injecting tasks.

The current implementation uses a lightweight djb2-based HMAC stub. Full HMAC-SHA256 is planned before the cluster feature exits experimental status.

## Peer discovery

| Method | Status |
|--------|--------|
| Static `~/.ahma/cluster/peers.json` | Working |
| mDNS (`_ahma-worker._tcp.local`) | Stub — logs a warning; full implementation requires the `mdns-sd` crate |
| Tailscale | Configure static peers using Tailscale hostnames |

## Scheduler behaviour

- Peers without the requested model are excluded.
- Among eligible peers, the one with the lowest `active_ops` is selected.
- If no peer is available, the orchestrator falls back to running the sub-task locally.

## Using via ahma_core

```rust
use ahma_core::{WorkerRegistry, ClusterScheduler, TaskManifest};

let registry = WorkerRegistry::new(60);  // 60s TTL
registry.load_static_peers()?;

let scheduler = ClusterScheduler::new(registry, "my-shared-key");

let manifest = TaskManifest {
    task_id: "sub_1".into(),
    prompt: "What are the key risks?".into(),
    model: "gemma4".into(),
    llm_base_url: "http://localhost:11434/v1".into(),
    max_tokens: 256,
    timeout_secs: 30,
    signature: String::new(),
};

if let Some(result) = scheduler.schedule(manifest).await {
    println!("Answer from {}: {}", result.worker_id, result.text);
}
```

## See also

- [docs/decompose.md](decompose.md) — sub-tasks that the scheduler dispatches
- [docs/task-vault.md](task-vault.md) — vault contents that travel with the task
- [SPEC.md](../SPEC.md) — cluster scheduler design notes
