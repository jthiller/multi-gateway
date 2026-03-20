mod api;
mod gateway_table;
mod keys_dir;
mod metrics;
mod settings;
mod udp_dispatcher;

use gateway_table::{downlink_channel, GatewayTable};
use keys_dir::KeysDir;
use settings::Settings;
use udp_dispatcher::UdpDispatcher;

use anyhow::Result;
use clap::Parser;
use gateway_rs::Region;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Response from /gateways endpoint
#[derive(Deserialize)]
struct GatewaysListResponse {
    gateways: Vec<GatewayInfoResponse>,
    total: usize,
    connected: usize,
}

/// Gateway info from API response
#[derive(Deserialize)]
struct GatewayInfoResponse {
    mac: String,
    public_key: String,
    connected: bool,
    connected_seconds: Option<u64>,
    last_uplink_seconds_ago: Option<u64>,
}

/// Response from /gateways/{mac}/sign endpoint
#[derive(Deserialize)]
struct SignResponseBody {
    signature: String,
}

/// Request body for signing
#[derive(serde::Serialize)]
struct SignRequestBody {
    data: String,
}

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to the settings file
    #[arg(short, long, default_value = "settings.toml")]
    config: PathBuf,
    #[clap(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, clap::Subcommand)]
enum Cmd {
    /// Gateway management commands
    #[clap(subcommand)]
    Gateways(Gateways),
    /// Start the multi-gateway server
    Run,
}

fn main() -> Result<()> {
    let cli = Args::parse();
    let settings = Settings::load(&cli.config)?;

    match cli.cmd {
        Cmd::Gateways(gateways) => {
            match gateways {
                Gateways::List => {
                    let url = format!("http://{}/gateways", settings.api_addr);
                    let client = reqwest::blocking::Client::new();
                    let mut request = client.get(&url);
                    if let Some(ref key) = settings.read_api_key {
                        request = request.header("X-API-Key", key);
                    }
                    let response = request.send().map_err(|e| {
                        anyhow::anyhow!("Failed to connect to server at {}: {}", url, e)
                    })?;

                    if !response.status().is_success() {
                        anyhow::bail!("Server returned error: {}", response.status());
                    }

                    let gateways: GatewaysListResponse = response
                        .json()
                        .map_err(|e| anyhow::anyhow!("Failed to parse response: {}", e))?;

                    println!(
                        "Gateways: {}/{} connected\n",
                        gateways.connected, gateways.total
                    );

                    for gw in gateways.gateways {
                        let status = if gw.connected {
                            "connected"
                        } else {
                            "disconnected"
                        };
                        println!("  {} [{}]", gw.mac, status);
                        println!("    public_key: {}", gw.public_key);
                        if let Some(secs) = gw.connected_seconds {
                            println!("    connected_for: {}s", secs);
                        }
                        if let Some(secs) = gw.last_uplink_seconds_ago {
                            println!("    last_uplink: {}s ago", secs);
                        }
                        println!();
                    }
                }
                Gateways::Sign { mac, data, format } => {
                    let url = format!("http://{}/gateways/{}/sign", settings.api_addr, mac);
                    let client = reqwest::blocking::Client::new();
                    let mut request = client.post(&url).json(&SignRequestBody { data });
                    if let Some(ref key) = settings.write_api_key {
                        request = request.header("X-API-Key", key);
                    }
                    let response = request.send().map_err(|e| {
                        anyhow::anyhow!("Failed to connect to server at {}: {}", url, e)
                    })?;

                    if !response.status().is_success() {
                        let status = response.status();
                        let body = response.text().unwrap_or_default();
                        anyhow::bail!("Server returned error {}: {}", status, body);
                    }

                    let sign_response: SignResponseBody = response
                        .json()
                        .map_err(|e| anyhow::anyhow!("Failed to parse response: {}", e))?;

                    match format {
                        SignOutputFormat::Qr => {
                            qr2term::print_qr(&sign_response.signature).map_err(|e| {
                                anyhow::anyhow!("Failed to generate QR code: {}", e)
                            })?;
                        }
                        SignOutputFormat::Base64 => {
                            println!("{}", sign_response.signature);
                        }
                    }
                }
            }
            Ok(())
        }
        Cmd::Run => {
            let _guard = setup_tracing(&settings.log);

            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;

            rt.block_on(run(settings))
        }
    }
}

async fn run(settings: Settings) -> Result<()> {
    let (shutdown_trigger, shutdown_listener) = triggered::trigger();

    // Handle Ctrl+C
    let shutdown_trigger_clone = shutdown_trigger.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            info!("received shutdown signal");
            shutdown_trigger_clone.trigger();
        }
    });

    // Initialize metrics
    metrics::init();

    // Parse region
    let region = parse_region(&settings.region);

    // Create keys directory manager
    let keys_dir = KeysDir::new(settings.keys_dir.clone());

    // Create downlink channel
    let (downlink_tx, downlink_rx) = downlink_channel(100);

    // Create gateway table (loads existing keys)
    let table = Arc::new(
        GatewayTable::new(
            keys_dir,
            settings.router.uri.clone(),
            settings.router.queue,
            downlink_tx,
            shutdown_listener.clone(),
        )
        .await?,
    );

    // Create UDP dispatcher
    let listen_addr = format!("{}:{}", settings.gwmp.addr, settings.gwmp.port);
    let disconnect_timeout = std::time::Duration::from_secs(settings.gwmp.disconnect_timeout);
    let dispatcher = UdpDispatcher::new(
        &listen_addr,
        table.clone(),
        downlink_rx,
        region,
        disconnect_timeout,
    )
    .await?;

    info!(
        udp_addr = %listen_addr,
        api_addr = %settings.api_addr,
        router_uri = %settings.router.uri,
        "multi-gateway server starting"
    );

    // Run UDP dispatcher and API server concurrently
    let dispatcher_handle = tokio::spawn(dispatcher.run(shutdown_listener.clone()));
    let api_handle = tokio::spawn(api::run_api_server(
        settings.api_addr.clone(),
        table.clone(),
        settings.read_api_key.clone(),
        settings.write_api_key.clone(),
        shutdown_listener,
    ));

    // Wait for either to finish
    tokio::select! {
        result = dispatcher_handle => {
            if let Err(e) = result {
                tracing::error!(error = %e, "UDP dispatcher task failed");
            }
        }
        result = api_handle => {
            if let Err(e) = result {
                tracing::error!(error = %e, "API server task failed");
            }
        }
    }

    info!("multi-gateway server stopped");
    Ok(())
}

fn parse_region(region_str: &str) -> Region {
    use gateway_rs::helium_proto::Region as ProtoRegion;
    use std::str::FromStr;

    if region_str.is_empty() {
        return Region::default();
    }

    match ProtoRegion::from_str(region_str) {
        Ok(proto_region) => {
            info!(region = %region_str, "using configured region");
            proto_region.into()
        }
        Err(_) => {
            tracing::warn!(
                region = %region_str,
                "invalid region, using default (unknown)"
            );
            Region::default()
        }
    }
}

fn setup_tracing(
    log_settings: &settings::LogSettings,
) -> tracing_appender::non_blocking::WorkerGuard {
    use tracing::Level;
    use tracing_subscriber::filter::LevelFilter;

    let (non_blocking, guard) = tracing_appender::non_blocking(std::io::stdout());

    let level: LevelFilter = match log_settings.level.to_lowercase().as_str() {
        "trace" => LevelFilter::TRACE,
        "debug" => LevelFilter::DEBUG,
        "info" => LevelFilter::INFO,
        "warn" => LevelFilter::WARN,
        "error" => LevelFilter::ERROR,
        _ => LevelFilter::INFO,
    };

    let filter = tracing_subscriber::filter::Targets::new()
        .with_target("helium_multi_gateway", level)
        .with_target("gateway_rs", level)
        .with_default(Level::WARN);

    let stdout_log = tracing_subscriber::fmt::layer()
        .compact()
        .with_writer(non_blocking);

    tracing_subscriber::registry()
        .with(stdout_log)
        .with(filter)
        .init();

    guard
}

#[derive(Debug, clap::Subcommand)]
pub(crate) enum Gateways {
    /// List all gateways and their status from the running server
    List,
    /// Sign data with a gateway's keypair and display as QR code
    Sign {
        /// Gateway MAC address (16 hex characters, e.g., AABBCCDDEEFF0011)
        mac: String,
        /// Base64-encoded data to sign
        data: String,
        /// Output format
        #[clap(long, default_value = "qr")]
        format: SignOutputFormat,
    },
}

#[derive(Debug, Clone, clap::ValueEnum)]
pub(crate) enum SignOutputFormat {
    /// Display signature as QR code in terminal
    Qr,
    /// Output signature as base64 string
    Base64,
}
