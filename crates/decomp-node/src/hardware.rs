//! What this device's hardware is, as the node tells the job board (spec
//! section 1, "Node profile": hardware fit is measured by "declared specs,
//! confirmed by benchmark jobs"). The job board offers it no more slots than
//! the hardware runs, and confirms the pace by timing jobs whose compute it
//! already knows.

use decomp_core::Hardware;
use sysinfo::System;

const BYTES_PER_MB: u64 = 1024 * 1024;

/// This device's cores and memory. GPUs are listed once the agent can run
/// inference on them.
pub fn read() -> Hardware {
    let mut system = System::new();
    system.refresh_memory();
    Hardware {
        cores: std::thread::available_parallelism().map_or(1, |cores| cores.get() as u32),
        memory_mb: system.total_memory() / BYTES_PER_MB,
        gpus: Vec::new(),
    }
}
