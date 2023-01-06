// copied from narwhal codebase's benchmark client
use anvil_rpc::request::RequestParams;
use ethers::prelude::*;
// Copyright(C) Facebook, Inc. and its affiliates.
use anyhow::{Context, Result};
use bytes::Bytes;
use clap::{crate_name, crate_version, App, AppSettings};
use env_logger::Env;
use futures::future::join_all;
use futures::sink::SinkExt as _;
use log::{info, warn};
use std::net::SocketAddr;
use tokio::net::TcpStream;
use tokio::time::{interval, sleep, Duration, Instant};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

// So looking at this I wonder if this works without selecting the broadcast_tx path
// since this interfaces directly with the mempool via a TCP connection we do not need to include the routes and create a http request
// We just need to pass in the Tx struct we need for making the anvil Request at the end.
// An idea is we adapt this same client to have two, one that sends actual http request and one that does not.
// I think we change the shim so all Tx types are sent anvil via a Tokio channel which directly forwards responses back to client having warp handle it all
//      eth_sendTransaction, eth_sendRawTransaction both send the Tx payload to a TCPStream to the mempool
// Once that is done batch Tx should be supported by the send Tx route -> aka a batched send Tx and a single Tx are accepted the same way

async fn get_dev_accounts(host: &SocketAddr) -> Result<Vec<(Address, Address)>> {
    let client = reqwest::Client::new();
    let res = client
        .get(format!(
            "{}/rpc_query",
            String::from("http://") + &host.to_string()
        ))
        .query(&[
            ("method", "eth_accounts".to_owned()),
            (
                "params",
                serde_json::to_string(&RequestParams::Array(vec![]))?,
            ),
        ])
        .send()
        .await?;

    let from = serde_json::from_slice::<Vec<Address>>(&res.bytes().await?).unwrap();
    let mut to = from.clone();
    to.rotate_right(1);
    let accounts = from.into_iter().zip(to.into_iter()).collect();
    Ok(accounts)
}

#[tokio::main]
async fn main() -> Result<()> {
    let matches = App::new(crate_name!())
        .version(crate_version!())
        .about("Benchmark client for Narwhal and Tusk.")
        .args_from_usage("<ADDR> 'The network address of the node where to send txs'")
        .args_from_usage("--rate=<INT> 'The rate (txs/s) at which to send the transactions'")
        .args_from_usage("--nodes=[ADDR]... 'Network addresses that must be reachable before starting the benchmark.'")
        .setting(AppSettings::ArgRequiredElseHelp)
        .get_matches();
    env_logger::Builder::from_env(Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();
    let target = matches
        .value_of("ADDR")
        .unwrap()
        .parse::<SocketAddr>()
        .context("Invalid socket address format")?;
    let rate = matches
        .value_of("rate")
        .unwrap()
        .parse::<u64>()
        .context("The rate of transactions must be a non-negative integer")?;
    let nodes = matches
        .values_of("nodes")
        .unwrap_or_default()
        .into_iter()
        .map(|x| x.parse::<SocketAddr>())
        .collect::<Result<Vec<_>, _>>()
        .context("Invalid socket address format")?;
    let accounts = get_dev_accounts(&target).await?;

    let client = Client {
        target,
        rate,
        nodes,
        accounts,
    };

    // Wait for all nodes to be online and synchronized.
    client.wait().await;

    // Start the benchmark.
    client.send().await.context("Failed to submit transactions")
}

struct Client {
    target: SocketAddr,
    rate: u64,
    nodes: Vec<SocketAddr>,
    accounts: Vec<(Address, Address)>,
}

impl Client {
    pub async fn send(&self) -> Result<()> {
        const PRECISION: u64 = 20; // Sample precision.
        const BURST_DURATION: u64 = 1000 / PRECISION;

        // Connect to the mempool.
        let stream = TcpStream::connect(self.target)
            .await
            .context(format!("failed to connect to {}", self.target))?;

        let mut txs: Vec<Vec<u8>> = Vec::new();
        for (from, to) in &self.accounts {
            txs.push(
                serde_json::to_vec(
                    &TransactionRequest::new()
                        .from(from.clone())
                        .to(to.clone())
                        .value(U256::from(1u64))
                        .gas(21000),
                )
                .unwrap(),
            )
        }
        assert_eq!(
            txs[0],
            serde_json::to_vec(
                &TransactionRequest::new()
                    .from(self.accounts[0].0)
                    .to(self.accounts[0].1)
                    .value(U256::from(1u64))
                    .gas(21000),
            )
            .unwrap()
        );

        info!("Node address: {}", self.target);

        // NOTE: This log entry is used to compute performance.
        info!("Transactions size: {} B", txs[0].len());

        // NOTE: This log entry is used to compute performance.
        info!("Transactions rate: {} tx/s", self.rate);

        // Submit all transactions.
        let burst = self.rate / PRECISION;
        let mut counter = 0;
        let mut transport = Framed::new(stream, LengthDelimitedCodec::new());
        let interval = interval(Duration::from_millis(BURST_DURATION));
        tokio::pin!(interval);

        // NOTE: This log entry is used to compute performance.
        info!("Start sending transactions");

        'main: loop {
            interval.as_mut().tick().await;
            let now = Instant::now();

            for x in 0..burst {
                let tx = Bytes::from(txs[(x as usize % txs.len())].clone());
                if x == counter % burst {
                    // NOTE: This log entry is used to compute performance.
                    info!("Sending sample transaction {}", counter);
                }

                // TODO: Check this indexing isn't the best and doesn't account for necessary wrap around.
                if let Err(e) = transport.send(tx).await {
                    warn!("Failed to send transaction: {}", e);
                    break 'main;
                }
            }
            if now.elapsed().as_millis() > BURST_DURATION as u128 {
                // NOTE: This log entry is used to compute performance.
                warn!("Transaction rate too high for this client");
            }
            counter += 1;
        }
        Ok(())
    }

    pub async fn wait(&self) {
        // Wait for all nodes to be online.
        info!("Waiting for all nodes to be online...");
        join_all(self.nodes.iter().cloned().map(|address| {
            tokio::spawn(async move {
                while TcpStream::connect(address).await.is_err() {
                    sleep(Duration::from_millis(10)).await;
                }
            })
        }))
        .await;
    }
}
