//! What this device's hardware is, as the node tells the job board (spec
//! section 1, "Node profile": hardware fit is measured by "declared specs,
//! confirmed by benchmark jobs"). The job board offers it no more slots than
//! the hardware runs, and confirms the pace by timing jobs whose compute it
//! already knows.

use decomp_core::{Gpu, Hardware};
use decomp_inference::decoder;
use sysinfo::System;

const BYTES_PER_MB: u64 = 1024 * 1024;

/// This device's cores and memory, the GPU models run on, and the approved
/// models and stages it holds.
pub fn read() -> Hardware {
    let mut system = System::new();
    system.refresh_memory();
    let memory_mb = system.total_memory() / BYTES_PER_MB;
    Hardware {
        cores: std::thread::available_parallelism().map_or(1, |cores| cores.get() as u32),
        memory_mb,
        gpus: gpu(memory_mb).into_iter().collect(),
        models: crate::models::held().ids(),
        stages: crate::models::held().stages(),
    }
}

/// The GPU models run on, if they run on one. Only that one is declared: a
/// machine with several GPUs runs a node for each.
fn gpu(memory_mb: u64) -> Option<Gpu> {
    let device = decoder::device();
    if device.is_cuda() {
        nvidia(&first_visible_cuda_device())
    } else if device.is_metal() {
        // An Apple GPU shares the machine's memory.
        apple_chip().map(|name| Gpu { name, memory_mb })
    } else {
        None
    }
}

/// The NVIDIA GPU the CUDA runtime calls its first: the first one listed in
/// `CUDA_VISIBLE_DEVICES`, by index or ID, or else GPU 0.
fn first_visible_cuda_device() -> String {
    std::env::var("CUDA_VISIBLE_DEVICES")
        .ok()
        .and_then(|list| list.split(',').next().map(|id| id.trim().to_string()))
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| "0".into())
}

/// An NVIDIA GPU's name and memory, from the driver's `nvidia-smi`.
fn nvidia(id: &str) -> Option<Gpu> {
    let output = std::process::Command::new("nvidia-smi")
        .args([
            &format!("--id={id}"),
            "--query-gpu=name,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    let line = String::from_utf8(output.stdout).ok()?;
    let (name, memory_mb) = line.trim().rsplit_once(',')?;
    Some(Gpu {
        name: name.trim().to_string(),
        memory_mb: memory_mb.trim().parse().ok()?,
    })
}

/// The name of the Apple chip, such as "Apple M2 Pro".
fn apple_chip() -> Option<String> {
    let output = std::process::Command::new("sysctl")
        .args(["-n", "machdep.cpu.brand_string"])
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    let name = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!name.is_empty()).then_some(name)
}
