//! Shared library for the `gpudog` client and the `gpudogd` daemon.

pub mod cli_options;
pub mod client;
pub mod config;
pub mod cuda_visible_devices;
pub mod daemon_log;
pub mod dashboard;
pub mod device_visibility;
pub mod gpu_inventory;
pub mod gpu_processes;
pub mod peer_credentials;
pub mod process_watcher;
pub mod protocol;
pub mod scheduler;
pub mod service;
