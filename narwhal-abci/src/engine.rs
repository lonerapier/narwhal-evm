use crate::AbciQueryQuery;
use ethers::prelude::*;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::convert::TryFrom;
use std::fmt::Debug;
use std::net::SocketAddr;
use tokio::sync::mpsc::Receiver;
use tokio::sync::oneshot::Sender as OneShotSender;
// Tendermint Types
use anvil_rpc::request::RpcMethodCall;
use tendermint_proto::abci::ResponseQuery;

// Narwhal types
use narwhal_crypto::Digest;
use narwhal_primary::Certificate;

pub struct Engine {
    /// The address of the ABCI app
    pub app_address: SocketAddr,
    /// The path to the Primary's store, so that the Engine can query each of the Primary's workers
    /// for the data corresponding to a Certificate
    pub store_path: String,
    /// Messages received from the ABCI Server to be forwarded to the engine.
    pub rx_abci_queries: Receiver<(OneShotSender<ResponseQuery>, AbciQueryQuery)>,
    /// The last block height, initialized to the application's latest block by default
    pub client: Provider<Http>,
    pub req_client: Provider<Http>,
}

impl Engine {
    pub fn new(
        app_address: SocketAddr,
        store_path: &str,
        rx_abci_queries: Receiver<(OneShotSender<ResponseQuery>, AbciQueryQuery)>,
    ) -> Self {
        // Instantiate a new client to not be locked in an Info connection

        let req_client =
            Provider::<Http>::try_from(String::from("http://") + &app_address.to_string()).unwrap();
        let client =
            Provider::<Http>::try_from(String::from("http://") + &app_address.to_string()).unwrap();

        Self {
            app_address,
            store_path: store_path.to_string(),
            rx_abci_queries,
            client,
            req_client,
        }
    }

    /// Receives an ordered list of certificates and apply any application-specific logic.
    pub async fn run(&mut self, mut rx_output: Receiver<Certificate>) -> eyre::Result<()> {
        // self.init_chain()?;
        loop {
            tokio::select! {
                Some(certificate) = rx_output.recv() => {
                    self.handle_cert(certificate).await?;
                },
                Some((tx, req)) = self.rx_abci_queries.recv() => {
                    self.handle_abci_query(tx, req).await?;
                }
                else => break,
            }
        }

        Ok(())
    }

    /// On each new certificate, increment the block height to proposed and run through the
    /// BeginBlock -> DeliverTx for each tx in the certificate -> EndBlock -> Commit event loop.
    async fn handle_cert(&mut self, certificate: Certificate) -> eyre::Result<()> {
        // drive the app through the event loop
        // self.begin_block(proposed_block_height)?;
        self.reconstruct_and_deliver_txs(certificate).await?;
        // self.end_block(proposed_block_height)?;
        // self.commit()?;
        Ok(())
    }

    /// Handles ABCI queries coming to the primary and forwards them to the ABCI App. Each
    /// handle call comes with a Sender channel which is used to send the response back to the
    /// Primary and then to the client.
    ///
    /// Client => Primary => handle_cert => ABCI App => Primary => Client
    async fn handle_abci_query(
        &mut self,
        tx: OneShotSender<ResponseQuery>,
        req: AbciQueryQuery,
    ) -> eyre::Result<()> {
        let rpc_call: RpcMethodCall = serde_json::from_str(&req.data).unwrap();

        let resp: Result<String, ProviderError> = self
            .req_client
            .request(&rpc_call.method, rpc_call.params)
            .await;

        let res = match resp {
            Ok(resp) => resp,
            Err(e) => {
                log::error!("Error in handle_abci_query: {:?}", e);
                // return Ok(());
                "error".to_string()
            }
        };

        let res_ser = serde_json::to_string(&res)?;

        if let Err(err) = tx.send(ResponseQuery {
            value: res_ser.into(),
            ..Default::default()
        }) {
            eyre::bail!("{:?}", err);
        }
        Ok(())
    }

    /// Opens a RocksDB handle to a Worker's database and tries to read the batch
    /// stored at the provided certificate's digest.
    fn reconstruct_batch(&self, digest: Digest, worker_id: u32) -> eyre::Result<Vec<u8>> {
        // Open the database to each worker
        // TODO: Figure out if this is expensive
        let db = rocksdb::DB::open_for_read_only(
            &rocksdb::Options::default(),
            self.worker_db(worker_id),
            true,
        )?;

        // Query the db
        let key = digest.to_vec();
        match db.get(&key) {
            Ok(Some(res)) => Ok(res),
            Ok(None) => eyre::bail!("digest {} not found", digest),
            Err(err) => eyre::bail!(err),
        }
    }

    async fn deliver_anvil_batch(&mut self, batch: Vec<u8>) -> eyre::Result<()> {
        // Deserialize and parse the message.
        match bincode::deserialize(&batch) {
            Ok(WorkerMessage::Batch(batch)) => {
                // batch.into_iter().try_for_each(|tx| {
                //     self.deliver_tx(tx)?;
                //     Ok::<_, eyre::Error>(())
                // })?;
                for tx in batch {
                    self.deliver_anvil_tx(tx).await?;
                }
            }
            _ => eyre::bail!("unrecognized message format"),
        };
        Ok(())
    }

    /// Reconstructs the batch corresponding to the provided Primary's certificate from the Workers' stores
    /// and proceeds to deliver each tx to the App over ABCI's DeliverTx endpoint.
    async fn reconstruct_and_deliver_txs(&mut self, certificate: Certificate) -> eyre::Result<()> {
        // Try reconstructing the batches from the cert digests
        //
        // NB:
        // This is maybe a false positive by Clippy, without the `collect` the Iterator fails
        // iterator fails to compile because we're mutably borrowing in the `try_for_each`
        // when we've already immutably borrowed in the `.map`.
        #[allow(clippy::needless_collect)]
        let batches = certificate
            .header
            .payload
            .into_iter()
            .map(|(digest, worker_id)| self.reconstruct_batch(digest, worker_id))
            .collect::<Vec<_>>();

        for batch in batches {
            let batch = batch?;
            self.deliver_anvil_batch(batch).await?;
        }

        Ok(())
    }

    /// Helper function for getting the database handle to a worker associated
    /// with a primary (e.g. Primary db-0 -> Worker-0 db-0-0, Wroekr-1 db-0-1 etc.)
    fn worker_db(&self, id: u32) -> String {
        format!("{}-{}", self.store_path, id)
    }
}

// Tendermint Lifecycle Helpers
impl Engine {
    /// Calls the `InitChain` hook on the app, ignores "already initialized" errors.
    // pub fn init_chain(&mut self) -> eyre::Result<()> {
    //     let mut client = ClientBuilder::default().connect(&self.app_address)?;
    //     match client.init_chain(RequestInitChain::default()) {
    //         Ok(_) => {}
    //         Err(err) => {
    //             // ignore errors about the chain being uninitialized
    //             if err.to_string().contains("already initialized") {
    //                 log::warn!("{}", err);
    //                 return Ok(());
    //             }
    //             eyre::bail!(err)
    //         }
    //     };
    //     Ok(())
    // }

    /// Calls the `BeginBlock` hook on the ABCI app. For now, it just makes a request with
    /// the new block height.
    // If we wanted to, we could add additional arguments to be forwarded from the Consensus
    // to the App logic on the beginning of each block.
    // fn begin_block(&mut self, height: i64) -> eyre::Result<()> {
    //     let req = RequestBeginBlock {
    //         header: Some(Header {
    //             height,
    //             ..Default::default()
    //         }),
    //         ..Default::default()
    //     };
    //
    //     self.client.begin_block(req)?;
    //     Ok(())
    // }

    /// Calls the `DeliverTx` hook on the ABCI app.
    // fn deliver_tx(&mut self, tx: Transaction) -> eyre::Result<()> {
    //     self.client.deliver_tx(RequestDeliverTx { tx })?;
    //     Ok(())
    // }

    async fn deliver_anvil_tx(&mut self, tx: Transaction) -> eyre::Result<()> {
        let anvil_tx: Transaction = tx.clone();
        let mut anvil_tx: TransactionRequest = match serde_json::from_slice(&anvil_tx) {
            Ok(tx) => tx,
            Err(err) => {
                // tracing::error!("could not decode request");
                eyre::bail!(err);
            }
        };

        // resolve the `to`
        match anvil_tx.to {
            Some(NameOrAddress::Address(addr)) => anvil_tx.to = Some(addr.into()),
            _ => panic!("not an address"),
        };

        let tx_receipt = self.client.send_transaction(anvil_tx, None).await?.await?;
        Ok(())
    }

    // / Calls the `EndBlock` hook on the ABCI app. For now, it just makes a request with
    // / the proposed block height.
    // If we wanted to, we could add additional arguments to be forwarded from the Consensus
    // to the App logic on the end of each block.
    // fn end_block(&mut self, height: i64) -> eyre::Result<()> {
    //     let req = RequestEndBlock { height };
    //     self.client.end_block(req)?;
    //     Ok(())
    // }

    // / Calls the `Commit` hook on the ABCI app.
    // fn commit(&mut self) -> eyre::Result<()> {
    //     self.client.commit()?;
    //     Ok(())
    // }
}

// Helpers for deserializing batches, because `narwhal::worker` is not part
// of the public API. TODO -> make a PR to expose it.
pub type Transaction = Vec<u8>;
pub type Batch = Vec<Transaction>;
#[derive(serde::Deserialize)]
pub enum WorkerMessage {
    Batch(Batch),
}

// #[derive(Serialize, Deserialize, Debug)]
// struct RpcJsonResponse<T>
// where
//     T: Serialize + DeserializeOwned,
// {
//     value: T,
// }
