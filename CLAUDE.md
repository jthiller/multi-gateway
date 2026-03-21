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

**GCP Project:** `helium-multi-gateway`
- **VM:** `helium-multi-gw`, e2-micro in `us-west1-b` (Oregon, close to AWS us-west-2 for low latency to Helium router)
- **Static IP:** `34.83.207.25` (reserved as `helium-gw-ip` in us-west1)
- **OS:** Ubuntu 24.04 LTS
- **DNS:** `hotspot.heliumtools.org` → `34.83.207.25` (Cloudflare, DNS only / gray cloud — required for UDP)

**Credentials:** API keys stored in `.env` (gitignored).

**Active instances:**
- **US915:** UDP 1680, API 4468, config `/etc/helium-multi-gateway/us915/settings.toml`, keys `/var/lib/helium-multi-gateway/keys/us915/`
- **EU868:** UDP 1681, API 4469, config `/etc/helium-multi-gateway/eu868/settings.toml`, keys `/var/lib/helium-multi-gateway/keys/eu868/`
- Additional regions use sequential ports (1682/4470 for AU915, etc.)

**Firewall rules:** `allow-helium-udp` (UDP 1680-1682), `allow-helium-api` (TCP 4468-4470).

**Multi-region:** Uses systemd template units (`helium-multi-gateway@<region>.service`) with per-region config dirs under `/etc/helium-multi-gateway/<region>/settings.toml` and separate key dirs under `/var/lib/helium-multi-gateway/keys/<region>/`.

**Backups:** Gateway keypairs backed up daily (3am UTC) to `gs://helium-multi-gateway-backups` via cron. 7-day lifecycle policy. Keypair loss requires re-provisioning on the Helium network.

**Monitoring:** GCP Ops Agent ships journald logs to Cloud Logging. Uptime check `helium-gw-health` pings `/metrics` on port 4468 every 5 minutes.

**Release artifacts:** `.deb` packages built by GitHub Actions on version tags (`v*`), downloaded via `gh release download`.

**Deploying updates:**
```bash
gh release download <tag> --dir /tmp/helium-gw-release
gcloud compute scp /tmp/helium-gw-release/*.deb helium-multi-gw:~/ --zone=us-west1-b --project=helium-multi-gateway
gcloud compute ssh helium-multi-gw --zone=us-west1-b --project=helium-multi-gateway \
  --command="sudo dpkg -i ~/helium-multi-gateway_*.deb && sudo systemctl restart helium-multi-gateway@us915 helium-multi-gateway@eu868"
```
