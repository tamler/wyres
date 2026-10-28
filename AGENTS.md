# AGENTS.md — build brief (Milestone 5: federation, v2)

**Read this before writing code.** Product vision lives in [docs/SPEC.md](docs/SPEC.md); locked defaults and milestone decisions in [docs/DECISIONS.md](docs/DECISIONS.md). This file is the scope for the current coding milestone.

## Status

Milestone 5: **complete** (definition of done met). Earlier: M1 local core (tamler/wyres#1), M2 mail checking + activity feed (tamler/wyres#2), M3 resource registry (tamler/wyres#3), M4 authenticated remote peer (tamler/wyres#4). Keep the [README.md](README.md) status current in every PR.

## Clean-room rules (mandatory)

- Implement from **this repository's docs** only. Federation prior art in SPEC (Matrix, ActivityPub, NATS leaf nodes, Tailscale) is for ideas only.
- **Do not** copy code, schemas, SQL, phrasing, or distinctive APIs from `mcp_agent_mail*`, forks, or any project with a non-permissive license rider.

## Locked defaults

Apache-2.0; agent names unique per project; `node_id` + append-only event log; owning node is law; never CRDT-merge ownership; free and open source always (self-hostable hub, no paid relay); never bind beyond loopback without auth; **never remote reap**; MSRV 1.88.

## Milestone 5 — in scope

1. **Per-node authority + keys** — Ed25519 key per node (created on first boot; older databases get one and their events signed on upgrade); every event signed; foreign events verified, projected, never allowed to rewrite local ownership.
2. **Addressing** — `name@project@node` (and `name@@node`); local addresses unchanged; agents act only on their own node.
3. **Pairing** — `wyres node name`, `wyres pair invite|join|revoke`, `wyres peers`; single-use invites carrying the hub's key; unique node names.
4. **Sync** — hub-and-spoke: `wyres serve --federation` (hub), `wyres sync [--watch]` (spokes dial out); push own events, pull a filtered stream with cursors; idempotent; signed node requests.
5. **Scoped permissions** — `read_feed`, `send_mail`, `borrow_resource`, enforced by the hub on push and by nodes before sending; no remote reap permission exists.
6. **Cross-node mail** (routed only to recipient nodes) and a **merged activity feed** (node-prefixed).
7. **Remote resources** — visible with owning node and reachability; borrowable only when reachable and permitted; lease reported to the owner; stop/reap only on the owning node.
8. **Path clashes** — overlapping holds on another node in a same-named project come back as warnings, never locks.
9. **Tests** — multiple nodes on one machine over real HTTP.
10. **Docs** — [docs/FEDERATION.md](docs/FEDERATION.md).

### Definition of done (Milestone 5)

- [x] Node keys, signed events, verification on ingest; owning node authoritative
- [x] `agent@project@node` addressing; acting agents must be local
- [x] `wyres pair invite|join|revoke`, `wyres peers`, `wyres node name|show`
- [x] `wyres serve --federation` hub endpoints; `wyres sync [--watch]` push/pull with cursors
- [x] Permissions `read_feed` / `send_mail` / `borrow_resource`; never remote reap
- [x] Cross-node mail, replies, receipts; merged activity feed
- [x] Remote resources visible; borrow only when reachable; reported to owner
- [x] Cross-node hold clashes as warnings
- [x] Multi-node integration tests on one machine (hub + 3 spokes), tampering, revocation, CLI
- [x] CI green: test + clippy (`-D warnings`) + fmt + MSRV 1.88
- [x] README status updated

## Explicitly OUT of scope

- Mesh topology; multiple hubs per spoke; end-to-end encryption of mail bodies (future)
- Cross-node requests and distributed path locks (by design)
- Wake adapters, Herdr plugin, release builds (Milestone 6)

## Keep the README status current

Whenever you complete a definition-of-done item or open a PR that changes milestone progress, update the **Status** section in [README.md](README.md) in the same PR.
