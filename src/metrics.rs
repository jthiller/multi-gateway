//! Prometheus metrics for gateway monitoring.

use crate::udp_dispatcher::DispatcherHealth;
use once_cell::sync::{Lazy, OnceCell};
use prometheus::{CounterVec, Encoder, IntGauge, Opts, Registry, TextEncoder};
use std::sync::Arc;

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

pub static DISPATCHER_LAST_EVENT_SECS: Lazy<IntGauge> = Lazy::new(|| {
    let gauge = IntGauge::new(
        "dispatcher_last_event_seconds_ago",
        "Seconds since the UDP dispatcher last processed an event",
    )
    .unwrap();
    REGISTRY.register(Box::new(gauge.clone())).unwrap();
    gauge
});

/// Shared reference to the dispatcher health state, set once at startup.
static DISPATCHER_HEALTH: OnceCell<Arc<DispatcherHealth>> = OnceCell::new();

/// Store the dispatcher health reference for Prometheus scraping.
pub fn set_dispatcher_health(health: Arc<DispatcherHealth>) {
    let _ = DISPATCHER_HEALTH.set(health);
}

/// Initialize all metrics (registers them with the registry).
/// Call this at startup to ensure metrics appear in /metrics even before first use.
pub fn init() {
    // Access each lazy static to force initialization and registration
    Lazy::force(&GATEWAY_CONNECTIONS);
    Lazy::force(&GATEWAY_DISCONNECTIONS);
    Lazy::force(&PACKETS_UPLINK);
    Lazy::force(&PACKETS_DOWNLINK);
    Lazy::force(&DISPATCHER_LAST_EVENT_SECS);
}

/// Gather all metrics and encode as Prometheus text format.
pub fn gather() -> String {
    // Update the dispatcher gauge at scrape time for an exact value.
    if let Some(health) = DISPATCHER_HEALTH.get() {
        let secs = health.seconds_since_last_event().unwrap_or(0);
        DISPATCHER_LAST_EVENT_SECS.set(secs as i64);
    }

    let encoder = TextEncoder::new();
    let metric_families = REGISTRY.gather();
    let mut buffer = Vec::new();
    encoder.encode(&metric_families, &mut buffer).unwrap();
    String::from_utf8(buffer).unwrap()
}
