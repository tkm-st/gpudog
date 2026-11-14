//! What the `gpudog` client and the `gpudogd` daemon share: the wire format
//! and the CUDA/NVML view of the devices. Depends on neither of them.

pub mod device_visibility;
pub mod nvml;
pub mod protocol;
