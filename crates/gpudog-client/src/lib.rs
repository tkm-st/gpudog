//! The client side of GPUDog: `gpudog run`, `health` and `dash`. Depends on
//! `gpudog-core`, never on the daemon.

pub mod cli_options;
pub mod client;
pub mod cuda_visible_devices;
pub mod dashboard;
