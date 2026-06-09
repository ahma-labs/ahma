# ahma_cluster Crate Specification

* **Status**: Approved
* **Date**: 2026-06-09

## 1. User Story / Problem Statement

*As a coordinator node in a local network or Tailscale mesh, I want to discover available peer workers, check their status, and securely dispatch subtasks to them so that I can scale heavy inference workloads across multiple machines.*

## 2. Acceptance Criteria

- **Peer Discovery**: Implements mDNS zero-config LAN discovery (`_ahma-worker._tcp.local.`) and static bootstrap via `~/.ahma/cluster/peers.json`.
- **Signed Dispatch**: Outbound task dispatch manifests and capabilities heartbeats are signed and verified with **HMAC-SHA256** using a cluster shared key.
- **Constant-Time Verification**: Uses constant-time comparison for heartbeat signatures to protect against timing side-channel attacks.
- **Scheduling & Load Scoring**: Schedules subtasks based on a relative load score factoring loaded models, active operations count, and free GPU VRAM.
- **TLS/mTLS Support**: Supports leaf certificate and private key generation via `ahma cluster cert init` for secure mTLS client authentication.
- **Transport Preference**: Prefers HTTP/3 (QUIC) for task dispatch, falling back to HTTP/2.

## 3. Non-Functional Requirements

- **Replay Protection**: Prevents task replay attacks using a local nonce cache.
- **AGPL Licensing**: Linking this crate enforces copyleft requirements (source disclosure) under AGPL-3.0-or-later.

## 4. Out of Scope

- Task scheduling over public, unauthenticated cloud endpoints.
