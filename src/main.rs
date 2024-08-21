use std::{net::SocketAddrV4, str::FromStr, time::Duration};

use clap::Parser;
use color_eyre::eyre::{Context, Result};
use opentelemetry_otlp::WithExportConfig;
use tokio::net::TcpListener;
use tracing::{field, info, trace_span, warn, Instrument};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

mod client;

// don't spend all day waiting for peers to respond
// may need tuning
pub(crate) const IDLE_TIMEOUT: Duration = Duration::from_secs(3);

// probably still to large for any valid terraria packet
// buuut it's not that much so ¯\_(ツ)_/¯
pub(crate) const MAX_BUFFER_LENGTH: usize = 1024 * 5;

#[derive(Debug, Parser)]
#[command(about, version)]
/// A very basic Terraria honeypot.
///
/// A Terraria honeypot, it will listen for connection requests,
/// occasionally request a password then scrape some basic data from the client
/// and send it to an opentelemetry endpoint.
struct Args {
    /// Honeypot address.
    ///
    /// The address the honeypot should bind to.
    /// (expected format: ip:port)
    #[arg(env)]
    address: SocketAddrV4,

    /// Password chance.
    ///
    /// Chance a password should be requested after connecting.
    /// (from 0.0 to 1.0)
    #[arg(env, short = 'p', default_value_t = 0.0)]
    password_chance: f32,

    #[group(flatten)]
    opentelemetry: OpenTelemetryArgs,
}

#[derive(Debug, Parser)]
struct OpenTelemetryArgs {
    /// OpenTelemetry endpoint.
    ///
    /// The opentelemetry endpoint to send traces to.
    #[arg(env = "OTEL_ENDPOINT", long = "otel-endpoint")]
    endpoint: Option<String>,

    /// OpenTelemetry headers.
    ///
    /// Extra headers to be sent to the opentelemetry endpoint.
    /// (expects the format of "key=val,key=val")
    #[arg(env = "OTEL_HEADERS", long = "otel-headers")]
    headers: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = setup()?;

    let listener = TcpListener::bind(args.address)
        .await
        .wrap_err("Failed to bind to address")?;

    info!("Server listening on {}", listener.local_addr()?);

    loop {
        let (stream, peer_addr) = listener.accept().await?;
        stream
            .set_nodelay(true)
            .wrap_err("Failed to set nodelay on peer")?;

        info!("New connection from: {peer_addr:?}");

        tokio::spawn(
            async move {
                match client::handle_client(stream, peer_addr, args.password_chance).await {
                    // todo
                    Ok(_client_info) => {
                        info!("Client disconnected.");
                    }
                    Err(error) => {
                        warn!("Client unexpectedly disconnected: {error}");
                    }
                }
            }
            .instrument(trace_span!(
                "client",
                %peer_addr,
                version = field::Empty,
                password = field::Empty,
                name = field::Empty,
                uuid = field::Empty
            )),
        );
    }
}

fn setup() -> Result<Args> {
    use opentelemetry::trace::TracerProvider as _;

    color_eyre::install()?;
    // console_subscriber::init();

    let args = Args::parse();

    // stdout logging layer set with RUST_LOG, default's to logging all info & higher events
    let registry = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer().with_filter(
            EnvFilter::builder()
                .with_default_directive(tracing::level_filters::LevelFilter::INFO.into())
                .from_env_lossy(),
        ),
    );

    // opentelemetry tracing layer if an otel endpoint is set, sends all trace & higher events
    if let Some(endpoint) = &args.opentelemetry.endpoint {
        let trace_config = opentelemetry_sdk::trace::Config::default()
            .with_resource(opentelemetry_sdk::Resource::new(vec![
                opentelemetry::KeyValue::new(
                    opentelemetry_semantic_conventions::resource::SERVICE_NAME,
                    "bottled_honey",
                ),
            ]))
            .with_sampler(opentelemetry_sdk::trace::Sampler::AlwaysOn);

        let trace_exporter = opentelemetry_otlp::new_exporter()
            .http()
            .with_http_client(reqwest::Client::new())
            .with_endpoint(endpoint);

        // extra headers to be sent
        // expects format of "key=val,key=val"
        // keys can't contain equal signs nd keys or values can't contain commas
        let trace_exporter = if let Some(headers) = &args.opentelemetry.headers {
            trace_exporter.with_headers(
                headers
                    .split(',')
                    .filter_map(|kv| {
                        kv.split_once('=')
                            .map(|(k, v)| (k.to_owned(), v.to_owned()))
                    })
                    .collect::<std::collections::HashMap<_, _>>(),
            )
        } else {
            trace_exporter
        };

        let tracer = opentelemetry_otlp::new_pipeline()
            .tracing()
            .with_trace_config(trace_config)
            .with_exporter(trace_exporter)
            .install_batch(opentelemetry_sdk::runtime::Tokio)?;

        let tracing_layer = tracing_opentelemetry::layer()
            .with_tracer(tracer.tracer("bottled_honey"))
            .with_filter(
                tracing_subscriber::filter::Targets::from_str("bottled_honey=trace").unwrap(),
            );

        registry.with(tracing_layer).init();
    } else {
        registry.init();
    }

    Ok(args)
}
