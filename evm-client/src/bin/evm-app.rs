use anvil::spawn;
use clap::Parser;
use std::net::SocketAddr;

#[derive(Debug, Clone, Parser)]
struct Args {
    #[clap(default_value = "0.0.0.0:26658")]
    host: String,
    #[clap(long, short)]
    demo: bool,
}

use tracing_error::ErrorLayer;

use tracing_subscriber::prelude::*;

/// Initializes a tracing Subscriber for logging
#[allow(dead_code)]
pub fn subscriber() {
    tracing_subscriber::Registry::default()
        .with(tracing_subscriber::EnvFilter::new("evm-app=trace"))
        .with(ErrorLayer::default())
        .with(tracing_subscriber::fmt::layer())
        .init()
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let args = Args::parse();
    // subscriber();

    let addr = args.host.parse::<SocketAddr>().unwrap();

    let config = anvil::NodeConfig {
        host: Some(addr.ip()),
        port: addr.port(),
        ..Default::default()
    };

    let (_, handle) = spawn(config).await;

    match handle.await.unwrap() {
        Err(err) => {
            dbg!(err);
        }
        Ok(()) => {
            println!("Listening on {}", addr);
        }
    }

    Ok(())
}
