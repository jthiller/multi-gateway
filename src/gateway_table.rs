//! Gateway state table with connection tracking and RAII task handles.
//!
//! Tracks all known gateways (from keys directory) and their runtime state:
//! - Connection status (online/offline)
//! - Connection duration
//! - Last uplink timestamp
//! - gRPC task handle (dropped on disconnect)

use crate::keys_dir::{mac_to_key_name, KeysDir};
use anyhow::Result;
use gateway_rs::{
    helium_crypto::Sign,
    helium_proto::services::router::envelope_down_v1,
    message_cache::MessageCache,
    semtech_udp::MacAddress,
    service::{packet_router::PacketRouterService, Reconnect},
    Keypair, PacketDown, PacketUp, PublicKey,
};
use http::Uri;
use serde::Serialize;
use std::{
    collections::HashMap,
    ops::Deref,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, info, warn};

const STORE_GC_INTERVAL: Duration = Duration::from_secs(60);

/// Downlink message to be sent back to a specific gateway
#[derive(Debug)]
pub struct DownlinkMessage {
    pub mac: MacAddress,
    pub packet: PacketDown,
}

pub type DownlinkSender = mpsc::Sender<DownlinkMessage>;
pub type DownlinkReceiver = mpsc::Receiver<DownlinkMessage>;

pub fn downlink_channel(capacity: usize) -> (DownlinkSender, DownlinkReceiver) {
    mpsc::channel(capacity)
}

/// Uplink message for a gateway instance
struct UplinkMessage {
    packet: PacketUp,
    received: Instant,
}

type UplinkSender = mpsc::Sender<UplinkMessage>;
type UplinkReceiver = mpsc::Receiver<UplinkMessage>;

/// Per-gateway state entry in the table
pub struct GatewayEntry {
    /// Gateway MAC address
    pub mac: MacAddress,
    /// Gateway keypair
    keypair: Arc<Keypair>,
    /// Whether currently connected via UDP
    connected: bool,
    /// When the gateway connected (if connected)
    connected_since: Option<Instant>,
    /// Last uplink timestamp
    last_uplink: Option<Instant>,
    /// Uplink sender (to send packets to the gRPC task)
    uplink_tx: Option<UplinkSender>,
    /// Task handle - dropping this cancels the gRPC task
    task_handle: Option<tokio::task::JoinHandle<()>>,
}

impl GatewayEntry {
    /// Create a new disconnected gateway entry
    fn new(mac: MacAddress, keypair: Arc<Keypair>) -> Self {
        Self {
            mac,
            keypair,
            connected: false,
            connected_since: None,
            last_uplink: None,
            uplink_tx: None,
            task_handle: None,
        }
    }

    /// Get the gateway's public key
    pub fn public_key(&self) -> &PublicKey {
        self.keypair.public_key()
    }

    /// Check if gateway is connected.
    /// Also detects if the gRPC task has died and marks as disconnected.
    pub fn is_connected(&mut self) -> bool {
        if self.connected {
            if let Some(ref handle) = self.task_handle {
                if handle.is_finished() {
                    let mac_name = mac_to_key_name(&self.mac);
                    warn!(mac = %mac_name, "gRPC task died unexpectedly, marking disconnected");
                    self.connected = false;
                    self.connected_since = None;
                    self.uplink_tx = None;
                    self.task_handle = None;
                    return false;
                }
            }
        }
        self.connected
    }

    /// Get connection duration if connected
    pub fn connection_duration(&self) -> Option<Duration> {
        self.connected_since.map(|since| since.elapsed())
    }

    /// Get time since last uplink
    pub fn time_since_last_uplink(&self) -> Option<Duration> {
        self.last_uplink.map(|t| t.elapsed())
    }

    /// Send an uplink packet.
    /// Returns false if the task is dead and needs restart.
    pub async fn send_uplink(&mut self, packet: PacketUp) -> bool {
        let now = Instant::now();
        self.last_uplink = Some(now);

        if !self.is_connected() {
            warn!(mac = %mac_to_key_name(&self.mac), "task dead, cannot send uplink");
            return false;
        }

        if let Some(ref tx) = self.uplink_tx {
            if tx
                .send(UplinkMessage {
                    packet,
                    received: now,
                })
                .await
                .is_err()
            {
                let mac_name = mac_to_key_name(&self.mac);
                warn!(mac = %mac_name, "uplink channel closed, task likely dead");
                self.connected = false;
                self.connected_since = None;
                self.uplink_tx = None;
                self.task_handle = None;
                return false;
            }
        }
        true
    }

    /// Start the gRPC task for this gateway
    fn start_task(
        &mut self,
        router_uri: Uri,
        queue_size: u16,
        downlink_tx: DownlinkSender,
        shutdown: triggered::Listener,
    ) {
        let mac_name = mac_to_key_name(&self.mac);
        info!(mac = %mac_name, "starting gRPC task");

        let (uplink_tx, uplink_rx) = mpsc::channel(queue_size as usize);
        self.uplink_tx = Some(uplink_tx);

        let task_handle = tokio::spawn(run_gateway_router(
            self.mac,
            self.keypair.clone(),
            router_uri,
            queue_size,
            uplink_rx,
            downlink_tx,
            shutdown,
        ));
        self.task_handle = Some(task_handle);
    }

    /// Stop the gRPC task (RAII - task is cancelled when handle is dropped)
    fn stop_task(&mut self) {
        let mac_name = mac_to_key_name(&self.mac);

        // Drop the uplink sender first to signal the task
        self.uplink_tx = None;

        // Abort the task if still running
        if let Some(handle) = self.task_handle.take() {
            handle.abort();
            info!(mac = %mac_name, "stopped gRPC task");
        }
    }

    /// Mark as connected and start gRPC task.
    /// If already connected, restarts the task to avoid stale state.
    fn connect(
        &mut self,
        router_uri: Uri,
        queue_size: u16,
        downlink_tx: DownlinkSender,
        shutdown: triggered::Listener,
    ) {
        if self.connected {
            let mac_name = mac_to_key_name(&self.mac);
            warn!(mac = %mac_name, "gateway reconnected while still marked connected, restarting task");
            self.stop_task();
        }
        self.connected = true;
        self.connected_since = Some(Instant::now());
        self.start_task(router_uri, queue_size, downlink_tx, shutdown);
    }

    /// Mark as disconnected and stop gRPC task
    fn disconnect(&mut self) {
        if self.connected {
            let mac_name = mac_to_key_name(&self.mac);
            let duration = self.connection_duration();
            info!(
                mac = %mac_name,
                duration_secs = ?duration.map(|d| d.as_secs()),
                "gateway disconnected"
            );

            self.connected = false;
            self.connected_since = None;
            self.stop_task();
        }
    }
}

/// Gateway information for API responses
#[derive(Debug, Serialize)]
pub struct GatewayInfo {
    pub mac: String,
    pub public_key: String,
    pub connected: bool,
    pub connected_seconds: Option<u64>,
    pub last_uplink_seconds_ago: Option<u64>,
}

impl From<&GatewayEntry> for GatewayInfo {
    fn from(entry: &GatewayEntry) -> Self {
        Self {
            mac: mac_to_key_name(&entry.mac),
            public_key: entry.public_key().to_string(),
            connected: entry.connected,
            connected_seconds: entry.connection_duration().map(|d| d.as_secs()),
            last_uplink_seconds_ago: entry.time_since_last_uplink().map(|d| d.as_secs()),
        }
    }
}

/// Table of all known gateways with their state
pub struct GatewayTable {
    /// Gateway entries by MAC address
    entries: RwLock<HashMap<MacAddress, GatewayEntry>>,
    /// Keys directory for loading/creating keypairs
    keys_dir: KeysDir,
    /// Router URI for gRPC connections
    router_uri: Uri,
    /// Queue size for packet buffering
    queue_size: u16,
    /// Downlink sender (shared with all gRPC tasks)
    downlink_tx: DownlinkSender,
    /// Shutdown listener
    shutdown: triggered::Listener,
}

impl GatewayTable {
    /// Create a new gateway table and load existing keys
    pub async fn new(
        keys_dir: KeysDir,
        router_uri: Uri,
        queue_size: u16,
        downlink_tx: DownlinkSender,
        shutdown: triggered::Listener,
    ) -> Result<Self> {
        let table = Self {
            entries: RwLock::new(HashMap::new()),
            keys_dir,
            router_uri,
            queue_size,
            downlink_tx,
            shutdown,
        };

        // Load existing keys
        table.load_existing_keys().await?;

        Ok(table)
    }

    /// Load all existing keys from the keys directory
    async fn load_existing_keys(&self) -> Result<()> {
        let keypairs = self.keys_dir.load()?;
        let mut entries = self.entries.write().await;

        for (name, keypair) in keypairs {
            // Parse MAC from filename (reverse of mac_to_key_name)
            if let Some(mac) = parse_mac_from_name(&name) {
                let entry = GatewayEntry::new(mac, Arc::new(keypair));
                entries.insert(mac, entry);
                info!(mac = %name, "loaded gateway key");
            } else {
                debug!(name = %name, "skipping non-MAC key file");
            }
        }

        info!(count = entries.len(), "loaded gateway keys");
        Ok(())
    }

    /// Handle a gateway connection event
    pub async fn on_connect(&self, mac: MacAddress) -> Result<()> {
        let mut entries = self.entries.write().await;

        // Get or create entry
        let entry = if let Some(entry) = entries.get_mut(&mac) {
            entry
        } else {
            // Auto-provision new gateway
            let keypair = Arc::new(self.keys_dir.get_or_create(&mac)?);
            let entry = GatewayEntry::new(mac, keypair);
            entries.insert(mac, entry);
            entries.get_mut(&mac).unwrap()
        };

        // Start gRPC task if not already connected
        entry.connect(
            self.router_uri.clone(),
            self.queue_size,
            self.downlink_tx.clone(),
            self.shutdown.clone(),
        );

        Ok(())
    }

    /// Handle a gateway disconnection event
    pub async fn on_disconnect(&self, mac: MacAddress) {
        let mut entries = self.entries.write().await;
        if let Some(entry) = entries.get_mut(&mac) {
            entry.disconnect();
        }
    }

    /// Send an uplink packet for a gateway
    pub async fn send_uplink(&self, mac: MacAddress, packet: PacketUp) -> Result<()> {
        let mut entries = self.entries.write().await;

        // Ensure gateway is connected
        let entry = if let Some(entry) = entries.get_mut(&mac) {
            entry
        } else {
            // Auto-provision if needed
            let keypair = Arc::new(self.keys_dir.get_or_create(&mac)?);
            let entry = GatewayEntry::new(mac, keypair);
            entries.insert(mac, entry);
            let entry = entries.get_mut(&mac).unwrap();
            entry.connect(
                self.router_uri.clone(),
                self.queue_size,
                self.downlink_tx.clone(),
                self.shutdown.clone(),
            );
            entry
        };

        if !entry.send_uplink(packet).await {
            // Task was dead, restart it
            let mac_name = mac_to_key_name(&mac);
            warn!(mac = %mac_name, "restarting dead gRPC task");
            entry.connect(
                self.router_uri.clone(),
                self.queue_size,
                self.downlink_tx.clone(),
                self.shutdown.clone(),
            );
        }
        Ok(())
    }

    /// Get public key for a gateway (creating entry if needed)
    pub async fn get_public_key(&self, mac: MacAddress) -> Result<PublicKey> {
        let mut entries = self.entries.write().await;

        let entry = if let Some(entry) = entries.get(&mac) {
            entry
        } else {
            // Auto-provision
            let keypair = Arc::new(self.keys_dir.get_or_create(&mac)?);
            let entry = GatewayEntry::new(mac, keypair);
            entries.insert(mac, entry);
            entries.get(&mac).unwrap()
        };

        Ok(entry.public_key().clone())
    }

    /// Get info for all gateways (for API)
    pub async fn list_gateways(&self) -> Vec<GatewayInfo> {
        let entries = self.entries.read().await;
        entries.values().map(GatewayInfo::from).collect()
    }

    /// Get info for a specific gateway
    pub async fn get_gateway(&self, mac: &MacAddress) -> Option<GatewayInfo> {
        let entries = self.entries.read().await;
        entries.get(mac).map(GatewayInfo::from)
    }

    /// Sign data using a gateway's keypair
    pub async fn sign(&self, mac: &MacAddress, data: &[u8]) -> Result<Option<Vec<u8>>> {
        let entries = self.entries.read().await;
        match entries.get(mac) {
            Some(entry) => {
                let signature = entry.keypair.sign(data)?;
                Ok(Some(signature))
            }
            None => Ok(None),
        }
    }
}

/// Parse a MAC address from a key filename
fn parse_mac_from_name(name: &str) -> Option<MacAddress> {
    // Name should be 16 hex characters (8 bytes)
    if name.len() != 16 {
        return None;
    }

    let bytes: Result<Vec<u8>, _> = (0..8)
        .map(|i| u8::from_str_radix(&name[i * 2..i * 2 + 2], 16))
        .collect();

    let bytes = bytes.ok()?;
    let mut arr = [0u8; 8];
    arr.copy_from_slice(&bytes);

    Some(MacAddress::from(arr))
}

/// Attempt to reconnect to the packet router, updating backoff and logging.
async fn try_reconnect(
    mac_name: &str,
    service: &mut PacketRouterService,
    reconnect: &mut Reconnect,
) {
    match service.reconnect().await {
        Ok(()) => {
            debug!(mac = %mac_name, "reconnected to router");
            reconnect.update_next_time(false);
        }
        Err(err) => {
            warn!(mac = %mac_name, error = %err, "failed to reconnect");
            reconnect.update_next_time(true);
        }
    }
}

/// Run the packet router loop for a single gateway
async fn run_gateway_router(
    mac: MacAddress,
    keypair: Arc<Keypair>,
    router_uri: Uri,
    queue_size: u16,
    mut uplink_rx: UplinkReceiver,
    downlink_tx: DownlinkSender,
    shutdown: triggered::Listener,
) {
    let mac_name = mac_to_key_name(&mac);
    info!(mac = %mac_name, uri = %router_uri, "starting gateway router");

    let mut service = PacketRouterService::new(router_uri.clone(), keypair);
    let mut store: MessageCache<PacketUp> = MessageCache::new(queue_size);
    let mut reconnect = Reconnect::default();

    loop {
        tokio::select! {
            _ = shutdown.clone() => {
                info!(mac = %mac_name, "gateway router shutting down");
                return;
            }

            msg = uplink_rx.recv() => {
                match msg {
                    Some(UplinkMessage { packet, received }) => {
                        store.push_back(packet, received);
                        if service.is_connected() {
                            if send_waiting_packets(&mac_name, &mut service, &mut store).await.is_err() {
                                service.disconnect();
                                warn!(mac = %mac_name, "router disconnected while sending");
                                reconnect.update_next_time(true);
                            }
                        } else if reconnect.wait().is_elapsed() {
                            // Service is disconnected and backoff has elapsed —
                            // proactively reconnect instead of waiting for the
                            // select! to pick the reconnect branch (which can be
                            // starved by a steady stream of uplinks).
                            try_reconnect(&mac_name, &mut service, &mut reconnect).await;
                        }
                    }
                    None => {
                        debug!(mac = %mac_name, "uplink channel closed, stopping router");
                        return;
                    }
                }
            }

            _ = reconnect.wait() => {
                try_reconnect(&mac_name, &mut service, &mut reconnect).await;
            }

            router_msg = service.recv() => {
                match router_msg {
                    Ok(envelope_down_v1::Data::Packet(packet)) => {
                        debug!(mac = %mac_name, "received downlink from router");
                        let packet_down: PacketDown = packet.into();
                        if downlink_tx.send(DownlinkMessage { mac, packet: packet_down }).await.is_err() {
                            warn!(mac = %mac_name, "downlink channel closed");
                            return;
                        }
                    }
                    Ok(envelope_down_v1::Data::SessionOffer(offer)) => {
                        match service.session_init(&offer.nonce).await {
                            Ok(()) => {
                                info!(mac = %mac_name, "session established");
                                reconnect.retry_count = reconnect.max_retries;
                                if send_waiting_packets(&mac_name, &mut service, &mut store).await.is_err() {
                                    service.disconnect();
                                    reconnect.update_next_time(true);
                                }
                            }
                            Err(err) => {
                                warn!(mac = %mac_name, error = %err, "session init failed");
                                service.disconnect();
                                reconnect.update_next_time(true);
                            }
                        }
                    }
                    Ok(envelope_down_v1::Data::PacketAck(_)) => {
                        debug!(mac = %mac_name, "received packet ack (ignored)");
                    }
                    Err(err) => {
                        warn!(mac = %mac_name, error = %err, "router error");
                        reconnect.update_next_time(true);
                    }
                }
            }
        }
    }
}

async fn send_waiting_packets(
    mac_name: &str,
    service: &mut PacketRouterService,
    store: &mut MessageCache<PacketUp>,
) -> gateway_rs::Result {
    use gateway_rs::helium_proto::services::router::PacketRouterPacketUpV1;

    while let (removed, Some(packet)) = store.pop_front(STORE_GC_INTERVAL) {
        if removed > 0 {
            info!(mac = %mac_name, removed, "discarded queued packets");
        }
        let mut uplink: PacketRouterPacketUpV1 = packet.deref().into();
        uplink.hold_time = packet.hold_time().as_millis() as u64;

        if let Err(err) = service.send_uplink(uplink).await {
            warn!(mac = %mac_name, error = %err, "failed to send uplink");
            store.push_front(packet);
            return Err(err);
        }
    }
    Ok(())
}
