// use crate::{BroadcastTxQuery, JsonRpcRequest};
// use anvil_rpc::response::QueryResponse;
use evm_client::types::{BroadcastTxQuery, JsonRpcRequest};
use eyre::WrapErr;
use futures::SinkExt;
use tendermint_proto::abci::ResponseQuery;
use tokio::sync::mpsc::Sender;
use tokio::sync::oneshot::{channel as oneshot_channel, Sender as OneShotSender};

use tokio::net::TcpStream;
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use warp::{Filter, Rejection};

use std::net::SocketAddr;

/// Simple HTTP API server which listens to messages on:
/// * `broadcast_tx`: forwards them to Narwhal's mempool/worker socket, which will proceed to put
/// it in the consensus process and eventually forward it to the application.
/// * `abci_query`: forwards them over a channel to a handler (typically the application).
pub struct RpcShim<T> {
    mempool_address: SocketAddr,
    tx: Sender<(OneShotSender<T>, JsonRpcRequest)>,
}

impl<T: Send + Sync + std::fmt::Debug> RpcShim<T> {
    pub fn new(
        mempool_address: SocketAddr,
        tx: Sender<(OneShotSender<T>, JsonRpcRequest)>,
    ) -> Self {
        Self {
            mempool_address,
            tx,
        }
    }
}

// TODO: replace these routes with traditional ETH routes -> send transaction
// TODO: See if we can just have all other routes 
// 2 options: 1) have benchmark Tx go into the 
impl RpcShim<ResponseQuery> {
    pub fn routes(self) -> impl Filter<Extract = impl warp::Reply, Error = Rejection> + Clone {
        let route_broadcast_tx = warp::path("broadcast_tx")
            .and(warp::query::<BroadcastTxQuery>())
            .and_then(move |req: BroadcastTxQuery| async move {
                log::warn!("broadcast_tx: {:?}", req);

                let stream = TcpStream::connect(self.mempool_address)
                    .await
                    .wrap_err(format!(
                        "ROUTE_BROADCAST_TX failed to connect to {}",
                        self.mempool_address
                    ))
                    .unwrap();
                let mut transport = Framed::new(stream, LengthDelimitedCodec::new());

                if let Err(e) = transport.send(req.tx.clone().into()).await {
                    Ok::<_, Rejection>(format!("ERROR IN: broadcast_tx: {:?}. Err: {}", req, e))
                } else {
                    Ok::<_, Rejection>(format!("broadcast_tx: {:?}", req))
                }
            });

        let route_rpc_query = warp::path("rpc_query")
            .and(warp::query::<JsonRpcRequest>())
            .and_then(move |req: JsonRpcRequest| {
                let tx_rpc_query = self.tx.clone();
                async move {
                    log::warn!("rpc_query: {:?}", req);

                    let (tx, rx) = oneshot_channel();
                    match tx_rpc_query.send((tx, req.clone())).await {
                        Ok(_) => {}
                        Err(err) => log::error!("Error forwarding rpc query: {}", err),
                    };

                    let resp = rx.await.unwrap();
                    // let resp_str = serde_json::to_string(&resp).unwrap();
                    // Return the value
                    Ok::<_, Rejection>(resp.value)
                }
            });

        route_broadcast_tx.or(route_rpc_query)
    }
}
