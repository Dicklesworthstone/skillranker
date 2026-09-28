//! Native endpoint projection shared by CLI allowance/readiness and ranking.
//! HTTP exchanges are implemented by the common JevClient, not a second client.

use super::endpoint::{EndpointConfig, EndpointError};

pub(crate) fn endpoint(account_id: &str) -> Result<EndpointConfig, EndpointError> {
    EndpointConfig::cloudflare(account_id)
}
