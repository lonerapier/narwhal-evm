mod shim_server;
pub use shim_server::RpcShim;

mod engine;
pub use engine::Engine;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BroadcastTxQuery {
    tx: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct JsonRpcRequest {
    method: String,
    params: String,
}
