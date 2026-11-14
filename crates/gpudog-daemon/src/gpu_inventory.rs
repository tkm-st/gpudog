//! What GPUs exist, and how to name them.
//!
//! Deliberately a trait. The scheduler must be testable on a machine with no
//! GPU, no driver and no container, so it depends on [`GpuInventory`], never
//! on NVML directly. [`NvmlInventory`] is the production implementation;
//! [`StaticInventory`] backs the tests and the `--num-gpus` override.

use nvml_wrapper::Nvml;

use gpudog_core::device_visibility::VisibleDevices;
use gpudog_core::nvml::{load_nvml, NvmlError};
use gpudog_core::protocol::{is_valid_gpu_uuid, same_uuid, sanitize_display_text, MAX_GPU_COUNT};

/// One device, as the daemon reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuDevice {
    /// Position in the inventory, in NVML (PCI bus) order. In the daemon's
    /// startup inventory ([`NvmlInventory::probe`]: every CUDA-visible
    /// device, managed or not) it is the scheduler slot: the process
    /// watcher reports it and the service reads it as the slot.
    /// [`select_devices`] renumbers only its own result, so never give that
    /// result to the service. Used internally and for dashboard metadata;
    /// `--gpus` numbers are resolved in the client's CUDA view instead.
    /// Not meaningful outside the daemon; never put it in
    /// `CUDA_VISIBLE_DEVICES`.
    pub index: u32,
    /// Stable across reboots, PCI reordering and container boundaries,
    /// unlike `index`. What the client exports as `CUDA_VISIBLE_DEVICES`.
    pub uuid: String,
    pub name: String,
    /// NVML's own index, as the daemon's namespace sees it. Logged only.
    pub nvml_index: u32,
}

/// Keeps the devices named in `allowed`, in their original order, and
/// renumbers `index` to `0..k-1`. `allowed` unset keeps every device (still
/// renumbered, which is the identity for a full NVML list).
///
/// Fails if an allowed UUID is not present: a daemon that silently managed
/// fewer GPUs than configured would look healthy while half the machine sat
/// idle.
pub fn select_devices(
    mut all: Vec<GpuDevice>,
    allowed: Option<&[String]>,
) -> Result<Vec<GpuDevice>, String> {
    if let Some(allowed) = allowed {
        if let Some(missing) = allowed
            .iter()
            .find(|uuid| !all.iter().any(|device| same_uuid(&device.uuid, uuid)))
        {
            return Err(format!("configured device {missing} was not found"));
        }
        all.retain(|device| allowed.iter().any(|uuid| same_uuid(&device.uuid, uuid)));
    }
    for (index, device) in (0u32..).zip(all.iter_mut()) {
        device.index = index;
    }
    Ok(all)
}

/// Read-only view of the machine's devices. Indices are dense and ascending:
/// `devices()[i].index == i`.
pub trait GpuInventory: Send + Sync {
    fn devices(&self) -> &[GpuDevice];

    /// Where this inventory came from, for the startup log line.
    fn source(&self) -> &str;

    fn device_count(&self) -> u32 {
        u32::try_from(self.devices().len()).unwrap_or(u32::MAX)
    }
}

/// A fixed device list. Used by tests, and by `gpudogd --num-gpus` on hosts
/// where NVML is unavailable or deliberately bypassed. Unlike
/// [`NvmlInventory`], it does not bound the list by `MAX_GPU_COUNT`; callers
/// parsing user input must.
#[derive(Debug, Clone)]
pub struct StaticInventory {
    devices: Vec<GpuDevice>,
}

impl StaticInventory {
    /// Fabricates `count` devices named after their index.
    pub fn with_count(count: u32) -> Self {
        Self {
            devices: (0..count)
                .map(|index| GpuDevice {
                    index,
                    uuid: format!("GPU-static-{index}"),
                    name: format!("static device {index}"),
                    nvml_index: index,
                })
                .collect(),
        }
    }

    pub fn new(devices: Vec<GpuDevice>) -> Self {
        Self { devices }
    }
}

impl GpuInventory for StaticInventory {
    fn devices(&self) -> &[GpuDevice] {
        &self.devices
    }

    fn source(&self) -> &str {
        "static"
    }
}

/// Inventory backed by a live NVML session, which is shut down when this is
/// dropped.
pub struct NvmlInventory {
    devices: Vec<GpuDevice>,
    // Keeps the session open for the inventory's lifetime; nvml-wrapper calls
    // `nvmlShutdown` exactly once on drop.
    _nvml: Nvml,
}

impl std::fmt::Debug for NvmlInventory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NvmlInventory")
            .field("devices", &self.devices)
            .finish()
    }
}

impl NvmlInventory {
    /// Loads NVML at runtime and enumerates every CUDA-visible device. There
    /// is deliberately no selecting variant: the daemon keeps every visible
    /// device as a slot, and `devices` only marks slots managed.
    pub fn probe() -> Result<Self, NvmlError> {
        let visible = VisibleDevices::probe().map_err(|e| NvmlError(e.0))?;
        Self::enumerate(load_nvml()?, visible.uuids())
    }

    fn enumerate(nvml: Nvml, visible: &[String]) -> Result<Self, NvmlError> {
        let count = visible.len() as u32;
        // The wire protocol cannot address an index at or above
        // `MAX_GPU_COUNT`, so a machine that big is a configuration error,
        // not something to slice.
        if count > MAX_GPU_COUNT {
            return Err(NvmlError(format!(
                "CUDA reports {count} visible devices, more than the protocol can address ({MAX_GPU_COUNT})"
            )));
        }
        let mut devices: Vec<GpuDevice> = visible
            .iter()
            .map(|visible_uuid| {
                let device = nvml.device_by_uuid(visible_uuid.as_str()).map_err(|e| {
                    NvmlError(format!(
                        "nvmlDeviceGetHandleByUUID failed for visible device {visible_uuid}: {e}"
                    ))
                })?;
                let index = device
                    .index()
                    .map_err(|e| NvmlError(format!("nvmlDeviceGetIndex failed: {e}")))?;
                let uuid = device.uuid().map_err(|e| {
                    NvmlError(format!("nvmlDeviceGetUUID failed for device {index}: {e}"))
                })?;
                if !same_uuid(&uuid, visible_uuid) {
                    return Err(NvmlError(format!(
                        "NVML UUID disagrees with CUDA device {visible_uuid}"
                    )));
                }
                let name = device.name().map_err(|e| {
                    NvmlError(format!("nvmlDeviceGetName failed for device {index}: {e}"))
                })?;
                // The UUID is exported into children's environments and
                // matched against config; refuse one that is not plainly one.
                if !is_valid_gpu_uuid(&uuid) {
                    return Err(NvmlError(format!(
                        "device {index} has an unexpected UUID '{}'",
                        sanitize_display_text(uuid.as_bytes())
                    )));
                }
                Ok(GpuDevice {
                    index,
                    uuid,
                    name,
                    nvml_index: index,
                })
            })
            .collect::<Result<_, NvmlError>>()?;
        devices.sort_by_key(|device| device.nvml_index);
        for (index, device) in (0u32..).zip(&mut devices) {
            device.index = index;
        }
        Ok(Self {
            devices,
            _nvml: nvml,
        })
    }
}

impl GpuInventory for NvmlInventory {
    fn devices(&self) -> &[GpuDevice] {
        &self.devices
    }

    fn source(&self) -> &str {
        "NVML"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_inventory_fabricates_the_requested_number_of_devices() {
        let inventory = StaticInventory::with_count(4);
        assert_eq!(inventory.device_count(), 4);
        assert_eq!(inventory.source(), "static");
        // Dense and ascending: the scheduler and CUDA_VISIBLE_DEVICES both
        // treat indices as positions.
        for (i, device) in inventory.devices().iter().enumerate() {
            assert_eq!(device.index as usize, i);
        }
    }

    #[test]
    fn static_inventory_supports_an_empty_machine() {
        let inventory = StaticInventory::with_count(0);
        assert_eq!(inventory.device_count(), 0);
        assert!(inventory.devices().is_empty());
    }

    #[test]
    fn static_inventory_preserves_an_explicit_device_list() {
        let device = |index, uuid: &str, name: &str| GpuDevice {
            index,
            uuid: uuid.into(),
            name: name.into(),
            nvml_index: index,
        };
        let inventory = StaticInventory::new(vec![
            device(0, "GPU-aaa", "device A"),
            device(1, "GPU-bbb", "device B"),
        ]);
        assert_eq!(inventory.device_count(), 2);
        assert_eq!(inventory.devices()[1].uuid, "GPU-bbb");
    }

    #[test]
    fn nvml_inventory_either_enumerates_devices_or_explains_itself() {
        let inventory = match NvmlInventory::probe() {
            Ok(inventory) => inventory,
            Err(error) => {
                assert!(!error.0.is_empty());
                eprintln!("NVML unavailable: {error}");
                return;
            }
        };
        assert_eq!(inventory.source(), "NVML");
        for (i, device) in inventory.devices().iter().enumerate() {
            assert_eq!(device.index as usize, i);
            assert!(!device.name.is_empty());
            // Conventionally "GPU-<uuid>"; an empty or truncated string would
            // break log correlation with nvidia-smi.
            assert!(is_valid_gpu_uuid(&device.uuid), "{:?}", device.uuid);
        }
        eprintln!("NVML reported {} device(s)", inventory.device_count());
    }

    #[test]
    fn nvml_inventory_can_be_created_and_destroyed_repeatedly() {
        // Guards the init/shutdown pairing: a probe after the previous
        // session was dropped must succeed too.
        for _ in 0..3 {
            match NvmlInventory::probe() {
                Ok(inventory) => {
                    assert_eq!(inventory.device_count() as usize, inventory.devices().len())
                }
                Err(_) => return,
            }
        }
    }

    fn uuids(devices: &[GpuDevice]) -> Vec<&str> {
        devices.iter().map(|d| d.uuid.as_str()).collect()
    }

    #[test]
    fn select_devices_keeps_everything_when_unset() {
        let all = StaticInventory::with_count(3).devices().to_vec();
        assert_eq!(select_devices(all.clone(), None).unwrap(), all);
    }

    #[test]
    fn select_devices_keeps_nvml_order_and_renumbers() {
        let all = StaticInventory::with_count(4).devices().to_vec();
        // Listed out of order and in a different case.
        let allowed = vec!["gpu-STATIC-3".to_owned(), "GPU-static-1".to_owned()];
        let selected = select_devices(all, Some(&allowed)).unwrap();
        assert_eq!(uuids(&selected), ["GPU-static-1", "GPU-static-3"]);
        assert_eq!(selected[0].index, 0);
        assert_eq!(selected[1].index, 1);
        assert_eq!(selected[0].nvml_index, 1);
        assert_eq!(selected[1].nvml_index, 3);
    }

    #[test]
    fn select_devices_refuses_a_missing_device() {
        let all = StaticInventory::with_count(2).devices().to_vec();
        let allowed = vec!["GPU-static-0".to_owned(), "GPU-missing".to_owned()];
        assert_eq!(
            select_devices(all, Some(&allowed)).unwrap_err(),
            "configured device GPU-missing was not found"
        );
    }

    #[test]
    fn inventories_are_usable_as_trait_objects() {
        let inventory: Box<dyn GpuInventory> = Box::new(StaticInventory::with_count(2));
        assert_eq!(inventory.device_count(), 2);
    }
}
