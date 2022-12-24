use anvil_rpc::request::RequestParams;
mod shim_server;
pub use shim_server::RpcShim;

mod engine;
pub use engine::Engine;

use serde::{Deserialize, Serialize};

// #[derive(Serialize, Deserialize, Debug, Clone)]
// pub struct BroadcastTxQuery {
//     tx: String,
// }
//
// #[derive(Serialize, Deserialize, Debug, Clone)]
// pub struct JsonRpcRequest {
//     pub method: String,
//     pub params: String,
// }
