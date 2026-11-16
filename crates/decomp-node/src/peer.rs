//! The HTTP calls a device makes to a node, as opposed to the coordinator.

use std::time::Duration;

/// How long one call to a node may take, including sending or receiving a blob.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(120);

pub(crate) fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        // Read refusals ourselves: their bodies say why.
        .http_status_as_error(false)
        .timeout_global(Some(TRANSFER_TIMEOUT))
        .build()
        .new_agent()
}
