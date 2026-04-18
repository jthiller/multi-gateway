//! UDP packet dispatcher for semtech-udp protocol.
//!
//! The UdpDispatcher receives all incoming UDP packets on a single port,
//! identifies the gateway by MAC address, and routes packets to the
//! appropriate gateway instance via the GatewayTable.

use crate::gateway_table::{DownlinkMessage, DownlinkReceiver, GatewayTable};
use crate::keys_dir::mac_to_key_name;
use anyhow::Result;
use gateway_rs::{
    semtech_udp::server_runtime::{Event, UdpRuntime},
    PacketUp, Region,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tracing::{debug, info, warn};

/// Shared dispatcher health state, readable from the API and metrics without
/// any locking contention with the dispatch loop.
pub struct DispatcherHealth {
    /// Unix epoch seconds when the dispatcher was created.
    started_epoch_secs: u64,
    /// Unix epoch seconds of the last event processed. 0 until the first event.
    last_event_epoch_secs: AtomicU64,
    /// Unix epoch seconds the dispatch loop last made progress — refreshed by
    /// real events and a periodic tick, so it advances whenever the loop is
    /// running, even with no traffic. This is the liveness signal the watchdog
    /// checks (distinct from last_event, which tracks real packets only).
    last_heartbeat_epoch_secs: AtomicU64,
    /// Total events processed (monotonic counter).
    events_processed: AtomicU64,
}

impl DispatcherHealth {
    fn new() -> Self {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            started_epoch_secs: now,
            last_event_epoch_secs: AtomicU64::new(0),
            last_heartbeat_epoch_secs: AtomicU64::new(now),
            events_processed: AtomicU64::new(0),
        }
    }

    fn record_event(&self) {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.last_event_epoch_secs.store(now, Ordering::Relaxed);
        self.last_heartbeat_epoch_secs.store(now, Ordering::Relaxed);
        self.events_processed.fetch_add(1, Ordering::Relaxed);
    }

    /// Refresh the loop-liveness heartbeat from the periodic dispatch-loop
    /// tick, so it advances even when no traffic is arriving.
    fn record_heartbeat(&self) {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.last_heartbeat_epoch_secs.store(now, Ordering::Relaxed);
    }

    /// Seconds since the last event was processed, or None if no event yet.
    pub fn seconds_since_last_event(&self) -> Option<u64> {
        let last = self.last_event_epoch_secs.load(Ordering::Relaxed);
        if last == 0 {
            return None;
        }
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Some(now.saturating_sub(last))
    }

    /// Seconds since the dispatch loop last made progress (an event or the
    /// periodic tick). Always available — the heartbeat is initialised at
    /// startup — so the watchdog detects a loop that never starts as well as
    /// one that wedges mid-run.
    pub fn seconds_since_last_heartbeat(&self) -> u64 {
        let last = self.last_heartbeat_epoch_secs.load(Ordering::Relaxed);
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        now.saturating_sub(last)
    }

    /// Seconds since the dispatcher was created.
    pub fn uptime_secs(&self) -> u64 {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        now.saturating_sub(self.started_epoch_secs)
    }

    /// Total events processed.
    pub fn total_events(&self) -> u64 {
        self.events_processed.load(Ordering::Relaxed)
    }
}

/// Default TX power for downlinks (in dBm)
const DEFAULT_TX_POWER: u32 = 27;

/// How often the dispatch loop refreshes its liveness heartbeat when idle.
/// Kept well below the watchdog's 60s warn threshold so a quiet but healthy
/// instance never looks stalled.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

/// UDP dispatcher for semtech-udp protocol.
pub struct UdpDispatcher {
    udp_runtime: UdpRuntime,
    table: Arc<GatewayTable>,
    downlink_rx: DownlinkReceiver,
    region: Region,
    health: Arc<DispatcherHealth>,
}

impl UdpDispatcher {
    pub async fn new(
        listen_addr: &str,
        table: Arc<GatewayTable>,
        downlink_rx: DownlinkReceiver,
        region: Region,
        disconnect_timeout: Duration,
    ) -> Result<Self> {
        info!(addr = %listen_addr, disconnect_timeout_secs = disconnect_timeout.as_secs(), "creating UDP dispatcher");
        let udp_runtime = UdpRuntime::new_with_disconnect_timeout(listen_addr, disconnect_timeout)
            .await
            .map_err(|e| anyhow::anyhow!("failed to bind UDP socket on {listen_addr}: {e}"))?;

        Ok(Self {
            udp_runtime,
            table,
            downlink_rx,
            region,
            health: Arc::new(DispatcherHealth::new()),
        })
    }

    /// Shared health state for the dispatcher, readable from the API and
    /// metrics without contention.
    pub fn health(&self) -> Arc<DispatcherHealth> {
        self.health.clone()
    }

    pub async fn run(mut self, shutdown: triggered::Listener) -> Result<()> {
        info!("UDP dispatcher starting");

        let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);

        loop {
            tokio::select! {
                _ = shutdown.clone() => {
                    info!("UDP dispatcher shutting down");
                    return Ok(());
                }

                _ = heartbeat.tick() => {
                    self.health.record_heartbeat();
                }

                event = self.udp_runtime.recv() => {
                    self.health.record_event();
                    if let Err(e) = self.handle_event(event).await {
                        warn!(error = %e, "error handling UDP event");
                    }
                }

                Some(downlink) = self.downlink_rx.recv() => {
                    self.health.record_event();
                    self.handle_downlink(downlink).await;
                }
            }
        }
    }

    async fn handle_event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::NewClient((mac, addr)) => {
                let mac_name = mac_to_key_name(&mac);
                crate::metrics::GATEWAY_CONNECTIONS
                    .with_label_values(&[&mac_name])
                    .inc();
                info!(mac = %mac_name, addr = %addr, "gateway connected");
                self.table.on_connect(mac).await?;
            }

            Event::PacketReceived(rxpk, mac) => {
                let mac_name = mac_to_key_name(&mac);

                // Get public key for packet parsing
                let public_key = self.table.get_public_key(mac).await?;

                // Parse the packet
                match PacketUp::from_rxpk(rxpk, &public_key, self.region) {
                    Ok(packet) if packet.is_uplink() => {
                        crate::metrics::PACKETS_UPLINK
                            .with_label_values(&[&mac_name])
                            .inc();
                        debug!(mac = %mac_name, "received uplink packet");
                        self.table.send_uplink(mac, packet).await?;
                    }
                    Ok(_packet) => {
                        debug!(mac = %mac_name, "ignoring non-uplink packet");
                    }
                    Err(err) => {
                        warn!(mac = %mac_name, error = %err, "failed to parse packet");
                    }
                }
            }

            Event::ClientDisconnected((mac, addr)) => {
                let mac_name = mac_to_key_name(&mac);
                crate::metrics::GATEWAY_DISCONNECTIONS
                    .with_label_values(&[&mac_name])
                    .inc();
                info!(mac = %mac_name, addr = %addr, "gateway disconnected (UDP keepalive timeout)");
                // This drops the gRPC task via RAII
                self.table.on_disconnect(mac).await;
            }

            Event::UpdateClient((mac, addr)) => {
                // Gateway's UDP source address changed. The semtech-udp runtime has
                // already updated its addr map so downlinks route correctly; the gRPC
                // task is independent of the UDP source and should not be restarted.
                // Common causes: Semtech reference packet-forwarders open two sockets
                // (upstream + downstream), NAT rebinding, or multi-homed gateways.
                let mac_name = mac_to_key_name(&mac);
                debug!(mac = %mac_name, addr = %addr, "gateway source address updated");
            }

            Event::StatReceived(stat, mac) => {
                let mac_name = mac_to_key_name(&mac);
                debug!(
                    mac = %mac_name,
                    rxnb = stat.rxnb,
                    rxok = stat.rxok,
                    txnb = stat.txnb,
                    "gateway stats received"
                );
            }

            Event::UnableToParseUdpFrame(err, _buf) => {
                warn!(error = %err, "failed to parse UDP frame");
            }

            Event::NoClientWithMac(_packet, mac) => {
                let mac_name = mac_to_key_name(&mac);
                warn!(mac = %mac_name, "downlink to unknown gateway");
            }
        }

        Ok(())
    }

    async fn handle_downlink(&mut self, downlink: DownlinkMessage) {
        let mac_name = mac_to_key_name(&downlink.mac);
        debug!(mac = %mac_name, "sending downlink to gateway");

        let txpk = match downlink.packet.to_rx1_pull_resp(DEFAULT_TX_POWER) {
            Ok(txpk) => txpk,
            Err(e) => {
                warn!(mac = %mac_name, error = %e, "failed to convert downlink to TxPk");
                return;
            }
        };

        let prepared = self.udp_runtime.prepare_downlink(txpk, downlink.mac);
        if let Err(e) = prepared.dispatch(Some(Duration::from_secs(5))).await {
            warn!(mac = %mac_name, error = %e, "failed to send downlink");
        } else {
            crate::metrics::PACKETS_DOWNLINK
                .with_label_values(&[&mac_name])
                .inc();
        }
    }
}
