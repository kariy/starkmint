use starkmint::app::StarknetApp;

use clap::Parser;
use color_eyre::{eyre::eyre, Result};
use tower::ServiceBuilder;
use tower_abci::{split, Server};
use tracing_subscriber::filter::LevelFilter;

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Cli {
    /// Bind the TCP server to this host.
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// Bind the TCP server to this port.
    #[arg(short, long, default_value = "26658")]
    port: u16,

    /// The default server read buffer size, in bytes, for each incoming client
    /// connection.
    #[arg(short, long, default_value = "1048576")]
    read_buf_size: usize,

    /// Increase output logging verbosity to DEBUG level.
    #[arg(short, long)]
    verbose: bool,

    /// Suppress all output logging (overrides --verbose).
    #[arg(short, long)]
    quiet: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli: Cli = Cli::parse();

    let log_level = if cli.quiet {
        LevelFilter::OFF
    } else if cli.verbose {
        LevelFilter::DEBUG
    } else {
        LevelFilter::INFO
    };

    tracing_subscriber::fmt().with_max_level(log_level).init();

    exec(&cli.host, cli.port).await;

    Ok(())
}

async fn exec(host: &str, port: u16) {
    // Construct our ABCI application.
    let service = StarknetApp::new();

    // Split it into components.
    let (consensus, mempool, snapshot, info) = split::service(service, 1);

    // Hand those components to the ABCI server, but customize request behavior
    // for each category -- for instance, apply load-shedding only to mempool
    // and info requests, but not to consensus requests.
    // Spawn a task to run the ABCI server
    let abci_server = tokio::task::spawn(
        Server::builder()
            .consensus(consensus)
            .snapshot(snapshot)
            .mempool(
                ServiceBuilder::new()
                    .load_shed()
                    .buffer(10)
                    .service(mempool),
            )
            .info(
                ServiceBuilder::new()
                    .load_shed()
                    .buffer(100)
                    .rate_limit(50, std::time::Duration::from_secs(1))
                    .service(info),
            )
            .finish()
            .unwrap()
            .listen(format!("{}:{}", host, port)),
    );

    tokio::select! {
        x = abci_server => x.unwrap().map_err(|e| eyre!(e)).unwrap(),
    };

    tracing::info!("ABCI server listening on {}::{}", host, port);
}
