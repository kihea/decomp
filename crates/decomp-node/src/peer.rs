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

/// Fetches the test data `probe` names from the node it measures, and returns
/// how long that took, in milliseconds, and the data's hash, which shows it was
/// fetched.
pub fn measure(probe: &decomp_core::Probe) -> Result<(u64, decomp_core::ContentHash), String> {
    let url = format!(
        "{}{}{}/{}",
        probe.address,
        decomp_core::PROBE_PATH,
        probe.seed,
        probe.bytes
    );
    let started = std::time::Instant::now();
    let mut response = agent()
        .get(&url)
        .call()
        .map_err(|error| error.to_string())?;
    let data = response
        .body_mut()
        .with_config()
        .limit(probe.bytes.saturating_add(1))
        .read_to_vec()
        .map_err(|error| error.to_string())?;
    let millis = started.elapsed().as_millis() as u64;
    Ok((millis, decomp_core::ContentHash::of(&data)))
}
