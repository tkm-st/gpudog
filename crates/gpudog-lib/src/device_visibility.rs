//! CUDA-environment device identity, without retaining a GPU context.
//!
//! CUDA, not NVML indices or a hand-written ordinal mapping, determines the
//! visible order. Call before initializing CUDA elsewhere and do not change
//! the process environment afterward: the driver caches its device view.

use cudarc::driver::{result, sys};
use nvml_wrapper::error::NvmlError;

use crate::gpu_inventory::load_nvml;
use crate::protocol::{self, Acquire, GpuSelection, Granted, MAX_GPU_COUNT};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct VisibilityError(pub String);

/// An ordered, validated snapshot. Position is the caller's CUDA ordinal.
/// Explicit construction supports alternative providers and deterministic
/// tests; production callers should use `probe` rather than inventing a view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleDevices {
    uuids: Vec<String>,
}

impl VisibleDevices {
    pub fn from_uuids(uuids: Vec<String>) -> Result<Self, VisibilityError> {
        if uuids.len() > MAX_GPU_COUNT as usize {
            return Err(VisibilityError("too many visible CUDA devices".into()));
        }
        for (i, uuid) in uuids.iter().enumerate() {
            if !protocol::is_valid_gpu_uuid(uuid)
                || uuids[..i]
                    .iter()
                    .any(|seen| protocol::same_uuid(seen, uuid))
            {
                return Err(VisibilityError(format!(
                    "invalid or duplicate visible GPU UUID '{}'",
                    protocol::sanitize_display_text(uuid.as_bytes())
                )));
            }
        }
        Ok(Self { uuids })
    }

    pub fn uuids(&self) -> &[String] {
        &self.uuids
    }

    /// Runtime-loads the CUDA driver, enumerates its visible devices, and
    /// rejects MIG through NVML. No context, stream or device memory is created.
    pub fn probe() -> Result<Self, VisibilityError> {
        if let Some(value) = std::env::var_os("CUDA_VISIBLE_DEVICES") {
            let text = value
                .to_str()
                .ok_or_else(|| VisibilityError("CUDA_VISIBLE_DEVICES is not valid UTF-8".into()))?;
            if text.split(',').any(|entry| {
                entry
                    .trim()
                    .get(..4)
                    .is_some_and(|p| p.eq_ignore_ascii_case("MIG-"))
            }) {
                return Err(VisibilityError(
                    "CUDA_VISIBLE_DEVICES contains an unsupported MIG device".into(),
                ));
            }
            if text.is_empty() || text == "-1" {
                return Self::from_uuids(Vec::new());
            }
        }
        // cudarc's lazy dynamic loader panics on unavailable libraries/symbols.
        // Contain that dependency failure; never fall back to NVML ordinals or
        // unrestricted GPUs. The process's existing panic hook is not changed.
        let uuids = std::panic::catch_unwind(cuda_uuids).map_err(|_| {
            VisibilityError("CUDA driver unavailable or missing required symbols".into())
        })??;
        if !uuids.is_empty() {
            let nvml = load_nvml().map_err(|e| VisibilityError(e.0))?;
            for uuid in &uuids {
                let device = nvml.device_by_uuid(uuid.as_str()).map_err(|e| {
                    VisibilityError(format!(
                        "cannot inspect visible device {uuid} through NVML: {e}"
                    ))
                })?;
                let reported = device.uuid().map_err(|e| {
                    VisibilityError(format!("cannot verify visible device {uuid}: {e}"))
                })?;
                if !protocol::same_uuid(uuid, &reported) {
                    return Err(VisibilityError(format!(
                        "NVML UUID disagrees with CUDA device {uuid}"
                    )));
                }
                let instance = match device.mig_is_mig_device_handle() {
                    Ok(value) => value,
                    Err(NvmlError::NotSupported) => false,
                    Err(e) => {
                        return Err(VisibilityError(format!(
                            "cannot inspect MIG state of {uuid}: {e}"
                        )))
                    }
                };
                let enabled = match device.mig_mode() {
                    Ok(mode) => mode.current != 0,
                    Err(NvmlError::NotSupported) => false,
                    Err(e) => {
                        return Err(VisibilityError(format!(
                            "cannot inspect MIG mode of {uuid}: {e}"
                        )))
                    }
                };
                if instance || enabled {
                    return Err(VisibilityError(format!(
                        "visible device {uuid} has unsupported MIG enabled"
                    )));
                }
            }
        }
        Self::from_uuids(uuids)
    }

    /// Resolves parsed local ordinals and constrains count requests. The result
    /// is a protocol-v6 request containing UUIDs only; an empty view is never
    /// interpreted as an unrestricted request.
    pub fn prepare_acquire(&self, request: &Acquire) -> Result<Acquire, VisibilityError> {
        let mut prepared = request.clone();
        match request.selection {
            GpuSelection::ExplicitList => {
                if !request.uuids.is_empty()
                    || !request.candidates.is_empty()
                    || request.gpus.len() != request.num_gpus as usize
                {
                    return Err(VisibilityError("inconsistent local GPU request".into()));
                }
                prepared.uuids = request
                    .gpus
                    .iter()
                    .map(|&index| {
                        self.uuids.get(index as usize).cloned().ok_or_else(|| {
                            VisibilityError(format!(
                                "CUDA number {index} is not visible ({} visible in this CUDA environment)",
                                self.uuids.len()
                            ))
                        })
                    })
                    .collect::<Result<_, _>>()?;
                prepared.gpus.clear();
                prepared.selection = GpuSelection::ExplicitUuids;
            }
            GpuSelection::ExplicitUuids => {
                for uuid in &request.uuids {
                    if !self
                        .uuids
                        .iter()
                        .any(|visible| protocol::same_uuid(visible, uuid))
                    {
                        return Err(VisibilityError(format!(
                            "{} is not visible in this CUDA environment",
                            protocol::sanitize_display_text(uuid.as_bytes())
                        )));
                    }
                }
            }
            GpuSelection::Count => {
                if !request.candidates.is_empty() {
                    Self::from_uuids(request.candidates.clone())?;
                }
                prepared.candidates = self
                    .uuids
                    .iter()
                    .filter(|uuid| {
                        request.candidates.is_empty()
                            || request
                                .candidates
                                .iter()
                                .any(|candidate| protocol::same_uuid(uuid, candidate))
                    })
                    .cloned()
                    .collect();
                if prepared.candidates.is_empty() {
                    return Err(VisibilityError(
                        "no GPUs are visible in this CUDA environment".into(),
                    ));
                }
                if prepared.candidates.len() < request.num_gpus as usize {
                    return Err(VisibilityError(format!(
                        "requested {} GPUs but only {} visible in this CUDA environment",
                        request.num_gpus,
                        prepared.candidates.len()
                    )));
                }
            }
        }
        protocol::encode(&protocol::Message::Acquire(prepared.clone()))
            .map_err(|e| VisibilityError(format!("invalid GPU request: {e}")))?;
        Ok(prepared)
    }

    pub fn validate_grant(
        &self,
        request: &Acquire,
        granted: &Granted,
    ) -> Result<(), VisibilityError> {
        protocol::encode(&protocol::Message::Granted(granted.clone()))
            .map_err(|e| VisibilityError(format!("invalid GPU grant: {e}")))?;
        if granted.uuids.len() != request.num_gpus as usize {
            return Err(VisibilityError(
                "daemon granted a different number of devices than requested".into(),
            ));
        }
        let allowed = if request.selection == GpuSelection::Count {
            &request.candidates
        } else {
            &request.uuids
        };
        for uuid in &granted.uuids {
            if !self.uuids.iter().any(|v| protocol::same_uuid(v, uuid))
                || !allowed.iter().any(|v| protocol::same_uuid(v, uuid))
            {
                return Err(VisibilityError(format!(
                    "daemon granted device {} outside the requested CUDA view",
                    protocol::sanitize_display_text(uuid.as_bytes())
                )));
            }
        }
        if request.selection == GpuSelection::ExplicitUuids
            && !request
                .uuids
                .iter()
                .zip(&granted.uuids)
                .all(|(a, b)| protocol::same_uuid(a, b))
        {
            return Err(VisibilityError(
                "daemon changed the requested device order".into(),
            ));
        }
        Ok(())
    }
}

fn cuda_uuids() -> Result<Vec<String>, VisibilityError> {
    if let Err(error) = result::init() {
        if error.0 == sys::CUresult::CUDA_ERROR_NO_DEVICE {
            return Ok(Vec::new());
        }
        return Err(VisibilityError(format!(
            "cuInit failed (CUDA status {})",
            error.0 as u32
        )));
    }
    let count = result::device::get_count().map_err(|e| {
        VisibilityError(format!(
            "cuDeviceGetCount failed (CUDA status {})",
            e.0 as u32
        ))
    })?;
    if count < 0 || count as u32 > MAX_GPU_COUNT {
        return Err(VisibilityError(
            "CUDA device count exceeds the supported range".into(),
        ));
    }
    (0..count)
        .map(|index| {
            let device = result::device::get(index).map_err(|e| {
                VisibilityError(format!(
                    "cuDeviceGet({index}) failed (CUDA status {})",
                    e.0 as u32
                ))
            })?;
            let uuid = result::device::get_uuid(device).map_err(|e| {
                VisibilityError(format!(
                    "cuDeviceGetUuid({index}) failed (CUDA status {})",
                    e.0 as u32
                ))
            })?;
            let hex = uuid
                .bytes
                .iter()
                .map(|b| format!("{:02x}", *b as u8))
                .collect::<String>();
            Ok(format!(
                "GPU-{}-{}-{}-{}-{}",
                &hex[..8],
                &hex[8..12],
                &hex[12..16],
                &hex[16..20],
                &hex[20..]
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests;
