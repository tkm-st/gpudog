//! The `gpudogd` daemon: scheduler, service, configuration, logging and GPU
//! observation. Depends on `gpudog-core`, never on the client.

pub mod config;
pub mod daemon_log;
pub mod gpu_inventory;
pub mod gpu_processes;
pub mod peer_credentials;
pub mod process_watcher;
pub mod scheduler;
pub mod service;
