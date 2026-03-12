//! Prometheus metrics for gateway monitoring.

use once_cell::sync::Lazy;
use prometheus::{CounterVec, Encoder, Opts, Registry, TextEncoder};

pub static REGISTRY: Lazy<Registry> = Lazy::new(Registry::new);

pub static GATEWAY_CONNECTIONS: Lazy<CounterVec> = Lazy::new(|| {
    let opts = Opts::new("gateway_connections_total", "Total gateway connections");
    let counter = CounterVec::new(opts, &["mac"]).unwrap();
    REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
});

pub static GATEWAY_DISCONNECTIONS: Lazy<CounterVec> = Lazy::new(|| {
    let opts = Opts::new(
        "gateway_disconnections_total",
        "Total gateway disconnections",
    );
    let counter = CounterVec::new(opts, &["mac"]).unwrap();
    REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
});

pub static PACKETS_UPLINK: Lazy<CounterVec> = Lazy::new(|| {
    let opts = Opts::new("packets_uplink_total", "Total uplink packets received");
    let counter = CounterVec::new(opts, &["mac"]).unwrap();
    REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
});

pub static PACKETS_DOWNLINK: Lazy<CounterVec> = Lazy::new(|| {
    let opts = Opts::new("packets_downlink_total", "Total downlink packets sent");
    let counter = CounterVec::new(opts, &["mac"]).unwrap();
    REGISTRY.register(Box::new(counter.clone())).unwrap();
    counter
});

/// Initialize all metrics (registers them with the registry).
/// Call this at startup to ensure metrics appear in /metrics even before first use.
pub fn init() {
    // Access each lazy static to force initialization and registration
    Lazy::force(&GATEWAY_CONNECTIONS);
    Lazy::force(&GATEWAY_DISCONNECTIONS);
    Lazy::force(&PACKETS_UPLINK);
    Lazy::force(&PACKETS_DOWNLINK);
}

/// Gather all metrics and encode as Prometheus text format.
pub fn gather() -> String {
    let encoder = TextEncoder::new();
    let metric_families = REGISTRY.gather();
    let mut buffer = Vec::new();
    encoder.encode(&metric_families, &mut buffer).unwrap();
    String::from_utf8(buffer).unwrap()
}
