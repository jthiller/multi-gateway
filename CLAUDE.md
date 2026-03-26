# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What This Is

Helium multi-gateway aggregator: accepts LoRaWAN gateway connections via Semtech UDP (GWMP), manages per-gateway Ed25519 keypairs, and routes packets to the Helium packet router over gRPC. Exposes a REST API for gateway management and Prometheus metrics.

## Build & Development

Default target is `x86_64-unknown-linux-musl` (set in `.cargo/config.toml`). Requires `protobuf-compiler` and `musl-tools` system packages.

```bash
cargo build                          # debug build (musl target)
cargo build --release                # release build
cargo deb --target x86_64-unknown-linux-musl  # build .deb package
```

## Linting & Formatting

```bash
cargo fmt -- --check                 # format check
cargo clippy --all-features -- -Dclippy::all  # lint
cargo sort --check                   # verify Cargo.toml dependency ordering
cargo test                           # run tests
```

## Running

```bash
helium-multi-gateway -c settings.toml run                    # start server
helium-multi-gateway -c settings.toml gateways list          # list gateways via API
helium-multi-gateway -c settings.toml gateways sign <MAC> <base64-data>  # sign data
```

## Architecture

The server runs three concurrent tasks in a `tokio::select!` loop (`main.rs`):

1. **UdpDispatcher** (`udp_dispatcher.rs`) — binds a UDP socket, receives Semtech GWMP packets, identifies gateways by MAC address, and routes packets through the GatewayTable. Also dispatches downlink packets back to gateways.

2. **GatewayTable** (`gateway_table.rs`) — central state: a `RwLock<HashMap<MacAddress, GatewayEntry>>`. Each entry holds the gateway's keypair, connection status, and an uplink channel. On connect, it spawns a per-gateway `run_gateway_router` tokio task that maintains a bidirectional gRPC stream to the Helium packet router (`PacketRouterService`). On disconnect, dropping the task handle cancels the gRPC stream (RAII).

3. **API Server** (`api.rs`) — Axum HTTP server with optional API key auth (separate read/write keys via middleware). Endpoints: `GET /gateways`, `GET /gateways/{mac}`, `POST /gateways/{mac}/sign`, `GET /metrics`.

Supporting modules:
- **KeysDir** (`keys_dir.rs`) — loads/creates Ed25519 keypairs on the filesystem, keyed by MAC address (e.g., `AABBCCDDEEFF0011.key`). Auto-provisions on first gateway connection.
- **Settings** (`settings.rs`) — TOML config: region, ports, router URI, API keys, logging.
- **Metrics** (`metrics.rs`) — Prometheus counters per gateway: connections, disconnections, uplink/downlink packets.

## Data Flow

```
LoRaWAN Gateway --[UDP/GWMP]--> UdpDispatcher --[mpsc]--> GatewayEntry --[gRPC stream]--> Helium Router
                <--[UDP/GWMP]-- UdpDispatcher <--[mpsc]-- GatewayEntry <--[gRPC stream]-- Helium Router
```

## Key Dependency

`gateway-rs` (custom fork at `github.com/lthiery/gateway-rs`) provides: Semtech UDP runtime, Helium crypto (keypairs, signing), packet router gRPC service, protobuf types, and message caching.

## Configuration

See `debian/settings.toml` for the default config. Region is a global per-instance setting (`US915`, `EU868`, `AU915`, `AS923_1`, `KR920`, `IN865`). Running multiple regions on one server requires multiple instances on different ports.

## Cross-Compilation

Cannot build the musl target on macOS — the `ring` crate requires `x86_64-linux-musl-gcc` which is not available on aarch64-apple-darwin. Use GitHub Actions (`release.yml`) to build `.deb` packages, or build natively on a Linux host.

## Deployment

**Release artifacts:** `.deb` packages built by GitHub Actions on version tags (`v*`). Install with `sudo dpkg -i helium-multi-gateway_*.deb`.

**Multi-region:** Run one instance per LoRaWAN region on sequential port pairs. Use systemd template units (`helium-multi-gateway@<region>.service`) with per-region config dirs:

| Region | UDP Port | API Port |
|---|---|---|
| US915 | 1680 | 4468 |
| EU868 | 1681 | 4469 |
| AU915 | 1682 | 4470 |

Each instance gets its own `settings.toml` and keys directory under `/etc/helium-multi-gateway/<region>/` and `/var/lib/helium-multi-gateway/keys/<region>/`.

**Keypair backups:** Gateway keypairs should be backed up regularly. Keypair loss requires re-provisioning on the Helium network.

**Operator-specific deployment details** (IPs, DNS, credentials, cloud project info) belong in `.env` or a local `DEPLOY.md`, both of which are gitignored.
