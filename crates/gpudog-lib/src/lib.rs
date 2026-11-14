//! Shared library for the `gpudog` client and the `gpudogd` daemon.
//!
//! Modules fall into three groups. Shared ones depend on no client or daemon
//! module; client and daemon modules never depend on each other.

// Shared: the wire format and the CUDA/NVML view of the devices.
pub mod device_visibility;
pub mod nvml;
pub mod protocol;

// Client: `gpudog run`, `health` and `dash`.
pub mod cli_options;
pub mod client;
pub mod cuda_visible_devices;
pub mod dashboard;

// Daemon: `gpudogd`.
pub mod config;
pub mod daemon_log;
pub mod gpu_inventory;
pub mod gpu_processes;
pub mod peer_credentials;
pub mod process_watcher;
pub mod scheduler;
pub mod service;
