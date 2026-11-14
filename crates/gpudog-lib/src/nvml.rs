//! Loading NVML, shared by the client's CUDA view and the daemon's
//! inventory and process observation.

use std::ffi::OsStr;

use nvml_wrapper::Nvml;

/// Why NVML could not be loaded or queried.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct NvmlError(pub String);

/// The versioned soname is what the NVIDIA container runtime injects; the bare
/// name only exists where the -dev package is installed. nvml-wrapper's own
/// default is the bare name, so it must be overridden.
const NVML_LIBRARIES: [&str; 2] = ["libnvidia-ml.so.1", "libnvidia-ml.so"];

/// Loads NVML at runtime and opens a session.
///
/// Loading is dynamic, so the binaries start on a host without the driver
/// and report a clear error instead of failing to load.
pub fn load_nvml() -> Result<Nvml, NvmlError> {
    let mut load_error = None;
    for library in NVML_LIBRARIES {
        match Nvml::builder().lib_path(OsStr::new(library)).init() {
            Ok(nvml) => return Ok(nvml),
            // Try the next name, but remember why the first one failed:
            // a library that exists but is broken (wrong arch, missing
            // dependency) is not the same as no driver at all.
            Err(nvml_wrapper::error::NvmlError::LibloadingError(error)) => {
                load_error.get_or_insert(error.to_string());
            }
            Err(error) => return Err(NvmlError(format!("nvmlInit failed: {error}"))),
        }
    }
    Err(NvmlError(format!(
        "libnvidia-ml.so.1 not found; is the NVIDIA driver present and, \
         in a container, was it started with --gpus? ({})",
        load_error.unwrap_or_default()
    )))
}
