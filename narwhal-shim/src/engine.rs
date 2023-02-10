use anvil_core::eth::EthRequest;
use ethers::prelude::*;
use evm_client::types::JsonRpcRequest;
use std::convert::TryFrom;
use std::net::SocketAddr;
use tokio::sync::mpsc::Receiver;
use tokio::sync::oneshot::Sender as OneShotSender;
// Tendermint Types
use tendermint_proto::abci::ResponseQuery;

// Narwhal types
use narwhal_crypto::Digest;
use narwhal_primary::Certificate;

pub struct Engine {
    /// The address of the EVM app
    pub app_address: SocketAddr,
    /// The path to the Primary's store, so that the Engine can query each of the Primary's workers
    /// for the data corresponding to a Certificate
    pub store_path: String,
    /// Messages received from the RPC Server to be forwarded to the engine.
    pub rx_rpc_queries: Receiver<(OneShotSender<ResponseQuery>, JsonRpcRequest)>,
    pub client: Provider<Http>,
    pub req_client: Provider<Http>,
}

impl Engine {
    pub async fn new(
        app_address: SocketAddr,
        store_path: &str,
        rx_rpc_queries: Receiver<(OneShotSender<ResponseQuery>, JsonRpcRequest)>,
    ) -> Self {
        let req_client =
            Provider::<Http>::try_from(String::from("http://") + &app_address.to_string()).unwrap();
        let client =
            Provider::<Http>::try_from(String::from("http://") + &app_address.to_string()).unwrap();

        Self {
            app_address,
            store_path: store_path.to_string(),
            rx_rpc_queries,
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
                Some((tx, req)) = self.rx_rpc_queries.recv() => {
                    self.handle_rpc_query(tx, req).await?;
                }
                else => break,
            }
        }

        Ok(())
    }

    /// On each new certificate, deliver the txs batch wise to anvil and then mine after
    /// successful delivery.
    /// Client => Primary => handle_cert => EVM App => Primary => Client
    async fn handle_cert(&mut self, certificate: Certificate) -> eyre::Result<()> {
        // drive the app through the event loop
        let num_txs = self.reconstruct_and_deliver_txs(certificate).await?;
        self.commit(num_txs).await?;

        Ok(())
    }

    /// Handles RPC queries coming to the primary and forwards them to the Anvil App. Each
    /// handle call comes with a Sender channel which is used to send the response back to the
    /// Primary and then to the client.
    ///
    /// Client => Primary => Anvil => Primary => Client
    async fn handle_rpc_query(
        &mut self,
        tx: OneShotSender<ResponseQuery>,
        req: JsonRpcRequest,
    ) -> eyre::Result<()> {
        let value: serde_json::Value = serde_json::from_str(&req.params)?;
        let method: String = req.method.clone();
        let rpc_req = serde_json::json!({
            "method": method.clone(),
            "params": value,
        });

        let res = match serde_json::from_value::<EthRequest>(rpc_req) {
            Ok(req) => self.client_request(req, &method, value).await?,
            Err(err) => {
                let err = err.to_string();
                if err.contains("unknown variant") {
                    log::error!("failed to deserialize method due to unknown variant");
                } else {
                    log::error!("failed to deserialize method");
                }
                err.into()
            }
        };

        if let Err(err) = tx.send(ResponseQuery {
            value: res,
            ..Default::default()
        }) {
            eyre::bail!("{:?}", err);
        }
        Ok(())
    }

    async fn client_request(
        &self,
        req: EthRequest,
        method: &str,
        params: serde_json::Value,
    ) -> eyre::Result<Vec<u8>> {
        match req {
            EthRequest::EthChainId(params) => {
                let res: String = self.client.request(method, params).await.unwrap();
                serde_json::to_vec(&res).map_err(Into::into)
            }
            EthRequest::EthAccounts(params) => {
                let res: Vec<Address> = self.client.request(method, params).await.unwrap();
                serde_json::to_vec(&res).map_err(Into::into)
            }
            EthRequest::EthBlockNumber(params) => {
                let res: u64 = self.client.request(method, params).await.unwrap();
                serde_json::to_vec(&res).map_err(Into::into)
            }
            EthRequest::EthGetBalance(_, _) => {
                let res: U256 = self.client.request(method, params).await.unwrap();
                serde_json::to_vec(&res).map_err(Into::into)
            }
            EthRequest::EthGetTransactionCount(_, _) => {
                let res: U256 = self.client.request(method, params).await.unwrap();
                serde_json::to_vec(&res).map_err(Into::into)
            }
            EthRequest::EthSign(_, _) => {
                let res: H256 = self.client.request(method, params).await.unwrap();
                serde_json::to_vec(&res).map_err(Into::into)
            }
            EthRequest::EthGetLogs(_) => {
                let res: H256 = self.client.request(method, params).await.unwrap();
                serde_json::to_vec(&res).map_err(Into::into)
            }
            _ => eyre::bail!("not implemented"),
        }
    }

    /// Opens a RocksDB handle to a Worker's database and tries to read the batch
    /// stored at the provided certificate's digest.
    fn reconstruct_batch(&self, digest: Digest, worker_id: u32) -> eyre::Result<(Vec<u8>, Digest)> {
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
            Ok(Some(res)) => Ok((res, digest)),
            Ok(None) => eyre::bail!("digest {} not found", digest),
            Err(err) => eyre::bail!(err),
        }
    }

    async fn deliver_batch(&mut self, batch: Vec<u8>, digest: Digest) -> eyre::Result<usize> {
        // Deserialize and parse the message.
        let mut len = 0;
        match bincode::deserialize(&batch) {
            Ok(WorkerMessage::Batch(batch)) => {
                len = batch.len();
                // log::info!("executing batch {}", digest);
                for tx in batch {
                    self.deliver_tx(tx).await?;
                }
            }
            _ => eyre::bail!("unrecognized message format"),
        };
        Ok(len)
    }

    /// Reconstructs the batch corresponding to the provided Primary's certificate from the Workers' stores
    /// and proceeds to deliver each tx to the App
    async fn reconstruct_and_deliver_txs(
        &mut self,
        certificate: Certificate,
    ) -> eyre::Result<(usize, Vec<Digest>)> {
        // Try reconstructing the batches from the cert digests
        //
        // NB:
        // This is maybe a false positive by Clippy, without the `collect` the Iterator fails
        // iterator fails to compile because we're mutably borrowing in the `try_for_each`
        // when we've already immutably borrowed in the `.map`.
        #[allow(clippy::needless_collect)]
        let batches= certificate
            .header
            .payload
            .into_iter()
            .map(|(digest, worker_id)| self.reconstruct_batch(digest, worker_id))
            .collect::<Vec<_>>();

        let mut len = 0;
        let mut digests = Vec::new();

        for batch in batches {
            let (batch , digest) = batch?;
            digests.push(digest.clone());
            len = len + self.deliver_batch(batch, digest).await?;
        }

        Ok((len, digests))
    }

    /// Helper function for getting the database handle to a worker associated
    /// with a primary (e.g. Primary db-0 -> Worker-0 db-0-0, Wroekr-1 db-0-1 etc.)
    fn worker_db(&self, id: u32) -> String {
        format!("{}-{}", self.store_path, id)
    }
}

impl Engine {
    // deliver tx to anvil as pending and then mine it later
    async fn deliver_tx(&mut self, tx: Transaction) -> eyre::Result<()> {
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

        let tx_receipt = self.client.send_transaction(anvil_tx, None).await?;
        // log::info!("tx_executed: {:?}", tx_receipt.as_ref().unwrap());
        // dbg!(tx_receipt);
        Ok(())
    }

    // / Calls the `Commit` hook to mine pending txs.
    async fn commit(&mut self, num_txs: (usize, Vec<Digest>)) -> eyre::Result<()> {
        let (num_txs, digests) = num_txs;
        if num_txs > 0 {
            log::info!("executing {} txs", num_txs);
            for digest in digests {
                log::info!("executing batch {}", digest);
            }
        }

        // let res = self
        //     .client
        //     .request("anvil_mine", vec![U256::from(num_txs as u64), U256::zero()])
        //     .await;
        let res = match self
            .client
            .request("anvil_mine", vec![U256::from(num_txs as u64), U256::zero()])
            .await
        {
            Ok(res) => res,
            Err(err) => {
                dbg!(&err);
                eyre::bail!(err);
            }
        };
        Ok(())
    }
}

// Helpers for deserializing batches, because `narwhal::worker` is not part
// of the public API. TODO -> make a PR to expose it.
pub type Transaction = Vec<u8>;
pub type Batch = Vec<Transaction>;
#[derive(serde::Deserialize)]
pub enum WorkerMessage {
    Batch(Batch),
}
