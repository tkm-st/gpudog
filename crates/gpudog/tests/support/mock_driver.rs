//! Standalone cdylib compiled by support/mod.rs, not linked into GPUDog.
//! Unsafe code is confined to the mock C ABI needed for subprocess tests.
use std::ffi::{c_char, c_void, CStr};

fn count() -> usize {
    std::env::var("MOCK_GPU_COUNT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4)
}

fn uuid(index: usize) -> String {
    format!("GPU-00000000-0000-0000-0000-{index:012x}")
}

fn visible() -> Vec<usize> {
    let mut all = (0..count()).collect::<Vec<_>>();
    if std::env::var("CUDA_DEVICE_ORDER").as_deref() == Ok("FASTEST_FIRST") {
        all.reverse();
    }
    let Ok(value) = std::env::var("CUDA_VISIBLE_DEVICES") else {
        return all;
    };
    let mut out = Vec::new();
    for entry in value.split(',').map(str::trim) {
        let found = if entry.to_ascii_uppercase().starts_with("GPU-") {
            (0..count()).find(|&i| uuid(i).eq_ignore_ascii_case(entry))
        } else {
            entry
                .parse::<usize>()
                .ok()
                .and_then(|i| all.get(i).copied())
        };
        match found {
            Some(i) if !out.contains(&i) => out.push(i),
            _ => break,
        }
    }
    out
}

unsafe fn text(value: &str, out: *mut c_char, size: u32) -> u32 {
    if out.is_null() || value.len() + 1 > size as usize {
        return 2;
    }
    std::ptr::copy_nonoverlapping(value.as_ptr(), out.cast(), value.len());
    *out.add(value.len()) = 0;
    0
}

#[no_mangle]
pub extern "C" fn cuInit(_: u32) -> u32 {
    if std::env::var("MOCK_CUDA_INIT_ERROR").is_ok() {
        1
    } else if std::env::var("MOCK_CUDA_NO_DEVICE").is_ok() {
        100
    } else {
        0
    }
}

#[no_mangle]
pub unsafe extern "C" fn cuDeviceGetCount(out: *mut i32) -> u32 {
    if std::env::var("MOCK_CUDA_COUNT_ERROR").is_ok() {
        return 999;
    }
    *out = visible().len() as i32;
    0
}

#[no_mangle]
pub unsafe extern "C" fn cuDeviceGet(out: *mut i32, ordinal: i32) -> u32 {
    match visible().get(ordinal as usize) {
        Some(&index) => {
            *out = index as i32;
            0
        }
        None => 101,
    }
}

#[no_mangle]
#[cfg(not(mock_missing_uuid))]
pub unsafe extern "C" fn cuDeviceGetUuid(out: *mut u8, device: i32) -> u32 {
    if device < 0 || device as usize >= count() {
        return 101;
    }
    let bytes = (device as u128).to_be_bytes();
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), out, 16);
    0
}

#[no_mangle]
pub extern "C" fn nvmlInit_v2() -> u32 {
    0
}
#[no_mangle]
pub extern "C" fn nvmlShutdown() -> u32 {
    0
}
#[no_mangle]
pub unsafe extern "C" fn nvmlDeviceGetCount_v2(out: *mut u32) -> u32 {
    *out = count() as u32;
    0
}
#[no_mangle]
pub unsafe extern "C" fn nvmlDeviceGetHandleByIndex_v2(index: u32, out: *mut *mut c_void) -> u32 {
    if index as usize >= count() {
        return 6;
    }
    *out = (index as usize + 1) as *mut c_void;
    0
}
#[no_mangle]
pub unsafe extern "C" fn nvmlDeviceGetHandleByUUID(
    value: *const c_char,
    out: *mut *mut c_void,
) -> u32 {
    let value = CStr::from_ptr(value).to_string_lossy();
    match (0..count()).find(|&i| uuid(i).eq_ignore_ascii_case(&value)) {
        Some(i) => {
            *out = (i + 1) as *mut c_void;
            0
        }
        None => 6,
    }
}
#[no_mangle]
pub unsafe extern "C" fn nvmlDeviceGetIndex(device: *mut c_void, out: *mut u32) -> u32 {
    *out = device as usize as u32 - 1;
    0
}
#[no_mangle]
pub unsafe extern "C" fn nvmlDeviceGetUUID(
    device: *mut c_void,
    out: *mut c_char,
    size: u32,
) -> u32 {
    if std::env::var("MOCK_UUID_MISMATCH").is_ok() {
        return text(&uuid(count()), out, size);
    }
    text(&uuid(device as usize - 1), out, size)
}
#[no_mangle]
pub unsafe extern "C" fn nvmlDeviceGetName(_: *mut c_void, out: *mut c_char, size: u32) -> u32 {
    text("Mock CUDA device", out, size)
}
#[no_mangle]
pub unsafe extern "C" fn nvmlDeviceIsMigDeviceHandle(_: *mut c_void, out: *mut u32) -> u32 {
    if std::env::var("MOCK_MIG_HANDLE_ERROR").is_ok() {
        return 999;
    }
    if std::env::var("MOCK_MIG_UNSUPPORTED").is_ok() {
        return 3;
    }
    *out = u32::from(std::env::var("MOCK_MIG_HANDLE").is_ok());
    0
}
#[no_mangle]
pub unsafe extern "C" fn nvmlDeviceGetMigMode(
    _: *mut c_void,
    current: *mut u32,
    pending: *mut u32,
) -> u32 {
    if std::env::var("MOCK_MIG_UNSUPPORTED").is_ok() {
        return 3;
    }
    if std::env::var("MOCK_MIG_FAILURE").is_ok() {
        return 999;
    }
    *current = u32::from(std::env::var("MOCK_MIG_ENABLED").is_ok());
    *pending = 0;
    0
}
#[no_mangle]
pub extern "C" fn nvmlErrorString(_: u32) -> *const c_char {
    b"mock NVML error\0".as_ptr().cast()
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MockProcess {
    pid: u32,
    memory: u64,
    gpu_instance: u32,
    compute_instance: u32,
}

unsafe fn compute_processes(device: *mut c_void, count: *mut u32, out: *mut MockProcess) -> u32 {
    if std::env::var("MOCK_GPU_OBSERVATION_FAILURE_FILE")
        .ok()
        .is_some_and(|p| std::path::Path::new(&p).exists())
    {
        return 999;
    }
    let mut rows = Vec::new();
    if let Ok(path) = std::env::var("MOCK_GPU_PROCESS_FILE") {
        if let Ok(text) = std::fs::read_to_string(path) {
            for line in text.lines() {
                let mut fields = line.split_whitespace();
                let gpu = fields.next().and_then(|s| s.parse::<usize>().ok());
                let pid = fields.next().and_then(|s| s.parse::<u32>().ok());
                if gpu == Some(device as usize - 1) {
                    if let Some(pid) = pid {
                        rows.push(MockProcess {
                            pid,
                            memory: 1024,
                            gpu_instance: u32::MAX,
                            compute_instance: u32::MAX,
                        });
                    }
                }
            }
        }
    }
    if out.is_null() || (*count as usize) < rows.len() {
        *count = rows.len() as u32;
        return if rows.is_empty() { 0 } else { 7 };
    }
    std::ptr::copy_nonoverlapping(rows.as_ptr(), out, rows.len());
    *count = rows.len() as u32;
    0
}

#[no_mangle]
pub unsafe extern "C" fn nvmlDeviceGetComputeRunningProcesses_v3(
    device: *mut c_void,
    count: *mut u32,
    out: *mut MockProcess,
) -> u32 {
    compute_processes(device, count, out)
}
#[no_mangle]
pub unsafe extern "C" fn nvmlDeviceGetComputeRunningProcesses_v2(
    device: *mut c_void,
    count: *mut u32,
    out: *mut MockProcess,
) -> u32 {
    compute_processes(device, count, out)
}
#[no_mangle]
pub unsafe extern "C" fn nvmlDeviceGetGraphicsRunningProcesses_v3(
    _: *mut c_void,
    count: *mut u32,
    _: *mut MockProcess,
) -> u32 {
    *count = 0;
    0
}
#[no_mangle]
pub unsafe extern "C" fn nvmlDeviceGetGraphicsRunningProcesses_v2(
    _: *mut c_void,
    count: *mut u32,
    _: *mut MockProcess,
) -> u32 {
    *count = 0;
    0
}
