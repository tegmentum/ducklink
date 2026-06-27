# Feasibility — WireGuard tunnel capability for DuckLink (wasm)

## Status

Draft / feasibility (read-only study completed). No code yet.

## Goal

Let a wasm DuckDB (DuckLink) reach **VPN-only / private endpoints** — a private
Postgres, an internal S3, a tunneled httpfs URL — by routing its scanner
connections (httpfs, s3fs, postgres_scanner, mysql_scanner) through a **WireGuard
tunnel**, reusing the existing `~/git/wireguard-wasm` component.

## Verdict

**Tractable, not blocked — but the load-bearing effort is a userspace TCP/IP
netstack, not a wire-in.** `wireguard-wasm` is complete and reusable as-is, but it
is *packet-only*: the project is "build a userspace netstack (smoltcp) + a custom
async `wasmtime-wasi` network backend, then hang the finished WG engine off it."

---

## 1. What `wireguard-wasm` provides — and the crux

A pure-Rust **userspace WireGuard protocol engine** as a `wasm32-wasip2` component
(`crate-type = cdylib`, package `wireguard:wasm`): Noise IKpsk2 / Curve25519
handshake, ChaCha20-Poly1305 transport, BLAKE2s, peer / allowed-ips routing,
keepalives, cookies, rekey, replay protection (`src/wireguard/mod.rs`, ~1037 lines).

WIT (`wit/wireguard.wit`, world `wireguard-component`) — the `wireguard-tunnel`
resource's load-bearing methods:
- `handle-outgoing(plaintext-ip-packet) -> write-to-network(encrypted-bytes)`
- `handle-incoming(encrypted-udp) -> write-to-tunnel(decrypted-ip-packet)`
- `tick()` — drives keepalives / handshakes / rekey

**The crux — there is no netstack.** The engine is the equivalent of a *TUN device
+ WG crypto*. It has **no `wasi:sockets`, no UDP, no TCP/IP, no smoltcp** (it parses
only the IP version nibble + destination bytes to pick a peer). The caller must
supply (a) the **outer UDP transport** to the WG endpoint and (b) a **source of
plaintext IP packets** for `handle-outgoing` plus a **sink** for the decrypted ones.

**Why it "just works" in v86 but not here.** `~/git/v86/crates/wireguard-core` is
`wireguard-wasm` repackaged as a plain Rust lib; v86 wraps it in
`crates/v86-devices/src/net_bridge/wireguard_adapter.rs` (TUN mode, L3). It works
there **only because the emulated x86 guest has its own kernel TCP/IP stack** that
produces the IP packets. DuckLink has no emulated guest, so it lacks the packet
source — that is the gap.

## 2. DuckLink's network seam

The in-core scanners (httpfs curl `HTTPUtil`, postgres_scanner libpq, mysql_scanner)
reach the network via wasi-libc BSD sockets -> `wasi:sockets` TCP, granted at **one
chokepoint**: the host `WasiCtxBuilder`
(`crates/ducklink-host/src/lib.rs:4747-4789`, `build_wasi_ctx_inherit` /
`_with_pipes` -> `inherit_network()` + `allow_ip_name_lookup`), feeding the
`duckdb-core` store (call sites ~5228 / 5407 / 5480). `inherit_network()` maps the
guest's sockets straight onto the host OS — **no interception point today**.
`wasi:sockets/udp` is available (the WIT deps are present).

A tunnel must sit *below* the scanner: scanner opens TCP to a private IP -> those
bytes become IP packets -> `handle-outgoing` -> encrypted UDP via
`wasi:sockets/udp` to the WG endpoint. Turning a TCP socket call into IP packets is
exactly what a netstack does.

## 3. Integration models

| Model | Verdict |
| --- | --- |
| **(i) Host network shim** — replace `inherit_network()` with a WG-routing network: for WG-allowed-ips destinations, terminate the guest TCP in an in-host **smoltcp** interface whose device is the tunnel (smoltcp -> `handle-outgoing` -> `wasi:sockets/udp`; incoming -> `handle-incoming` -> smoltcp -> guest TCP); non-WG destinations fall through to `inherit_network()` (split tunnel). | **RECOMMENDED.** The only model that transparently tunnels the existing in-core scanners. Tractable but real: the new piece is smoltcp <-> WG <-> a custom async `wasmtime-wasi` network backend. |
| **(ii) compose:dynlink resident network provider** — a `tunneled-connect` capability (model on s3-endpoint). | Viable for *new/component* scanners only; the in-core scanners use wasi-libc sockets and wouldn't route through it without recompiling. Still needs the netstack. |
| **(iii) `duckdb:extension` SQL surface** (`wg_connect`/`wg_status`). | Easy but **cosmetic** — management/observability only; no traffic routing. Good companion to (i), not a substitute. |

**Recommended:** (i) the host shim, with (iii) added later for SQL-surface management.

## 4. The netstack is reusable infrastructure (the key architectural point)

The missing piece — a **userspace TCP/IP netstack + a custom async `wasmtime-wasi`
network backend over a pluggable packet device** — is **DB-agnostic and broadly
reusable**, not a DuckLink-specific build:

- **DuckLink** needs it for the WG tunnel (scanners -> smoltcp -> WG).
- **sqlink** needs the identical thing for its scanners over WG.
- **v86** already has the seed: `net_bridge/{tcp_gateway,packet_net_adapter}.rs`
  (a working smoltcp integration) + `wireguard_adapter.rs` (a TUN-mode WG adapter)
  + a `smoltcp` dependency. v86 uses it in the *opposite* direction (NAT guest IP
  out to host sockets), but the netstack + packet-device + WG-adapter machinery is
  the same.
- Any future "wasm host with `wasi:sockets` guests that needs a synthetic /
  tunneled / policy-routed network" is the same capability.

The natural shape is a **packet-device trait** (`wireguard-wasm` is one device; the
v86 emulated NIC is another; a plain host-UDP relay is a third) with two consumers:
a **smoltcp netstack** above it, and a **custom `wasmtime-wasi` network backend**
that routes a guest's `wasi:sockets` through it (the host-shim direction) — while
v86 keeps the guest-NIC-to-host-sockets direction.

### Recommendation: a top-level repo

Make it a focused standalone repo — alongside `wireguard-wasm`, `s3-wasm`,
`sqlite-wasm`, `python-wasm` — e.g. **`~/git/netstack-wasm`** (a "userspace netstack
+ wasi network shim"). It would:
1. **Lift v86's `net_bridge`** (smoltcp integration + packet-device + the WG adapter)
   as the seed — consolidating the netstack that today lives only in v86.
2. Add the **custom async `wasmtime-wasi` network backend** (route guest
   `wasi:sockets` through a packet device) — the new piece DuckLink/sqlink need and
   v86 does not.
3. Define the **packet-device trait** (WG via `wireguard-wasm`, v86 NIC, host-UDP).

Consumed by **v86** (its `net_bridge` -> the shared repo), **ducklink** (the WG host
shim), and **sqlink** (same). This mirrors the datalink-consolidation philosophy —
lift the DB-agnostic, cross-repo infrastructure once — but as a *networking*
capability broader than the DB-ecosystem `datalink` crates (it serves v86, which is
not a DB), so a standalone repo fits better than a `datalink` crate.

## 5. Risks

- **Netstack is mandatory and is the real build** (no kernel TUN on wasm) — budget
  it as the effort, not glue.
- **Async WASI linker** — a synthetic network (UDP outer + smoltcp poll loop + the
  guest socket) needs the async linker (the s3-endpoint resident-provider finding);
  the host's current path is the simple sync `inherit_network()`.
- **`wasi:sockets/udp`** — available, but DuckLink has only exercised TCP; the UDP
  datagram path to the WG endpoint is new to validate.
- **TLS-over-WG** — postgres/mysql/httpfs TLS runs *inside* the tunneled TCP; fine
  (just bytes over smoltcp TCP), but double crypto (TLS + ChaCha20) cost.
- **Per-route / split tunnel** — only allowed-ips subnets through WG; the allowed-ips
  matching already exists in `wireguard-wasm`; the host must consult it.
- **Custom `wasmtime-wasi` socket backend** — the integration-risk concentration
  (DNS / connect / error semantics for libpq + curl).

## 6. Reuse inventory

- `wireguard-wasm` in full (zero changes; via WIT or the `wireguard-core` Rust lib).
- v86 `net_bridge/wireguard_adapter.rs` (TUN-mode template) + `tcp_gateway.rs` /
  `packet_net_adapter.rs` (working smoltcp examples) — the seed for `netstack-wasm`.
- The host network grant chokepoint (`build_wasi_ctx_inherit`/`_with_pipes`) — the
  swap point for the WG-routing network.
- The s3-endpoint resident provider + compose:dynlink async runtime — the async
  resident pattern the shim needs.
- The UDP WIT deps + the scanners' TCP infra.

## 7. Build plan (native host first)

1. **Native PoC, no DuckDB** — drive `wireguard-core` from a Rust harness; complete
   a real handshake to a live WG peer over a UDP socket (lift v86's adapter).
2. **Add smoltcp** — a smoltcp interface whose device is the tunnel; open one TCP
   connection through it to a private IP. **Highest-risk step — closes the netstack
   gap.** (This is the seed of `netstack-wasm`.)
3. **Host shim, one scanner** — replace `inherit_network()` for the `duckdb-core`
   store with a network that routes allowed-ips destinations through step-2's
   smoltcp+WG (on the async linker), falling through otherwise; target
   postgres_scanner to a VPN-only Postgres (cleanest single TCP+TLS stream).
4. **Generalize** — mysql / httpfs / s3, wire allowed-ips split-tunnel routing, then
   add the `wg_connect`/`wg_status` SQL surface as a `duckdb:extension` component.

**Scope honesty:** steps 1 + 4 are small; **steps 2-3 (smoltcp <-> WG <-> synthetic
async `wasmtime-wasi` socket) are the genuine build — weeks, not days.**
