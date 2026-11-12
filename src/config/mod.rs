//! Client configurations.

pub mod data;
pub mod exec;

pub use data::TxflowDataClientConfig;
pub use exec::TxflowExecClientConfig;

use crate::common::{
    enums::TxflowEnvironment,
    urls::{txflow_api_ws_url, txflow_ws_url, ws_urls_from_http},
};

/// `(node stream URL, API-service stream URL)`. One host serves HTTP and both sockets, so an
/// overridden HTTP base URL moves the sockets with it.
pub(crate) fn ws_urls(
    environment: TxflowEnvironment,
    base_url_http: Option<&str>,
) -> (String, String) {
    base_url_http.map_or_else(
        || {
            (
                txflow_ws_url(environment).to_string(),
                txflow_api_ws_url(environment).to_string(),
            )
        },
        ws_urls_from_http,
    )
}
