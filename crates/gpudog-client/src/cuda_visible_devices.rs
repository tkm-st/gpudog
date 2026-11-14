//! UUID list rendering/parsing. Visibility and ordinal resolution belong to
//! `device_visibility`; this module never replaces inherited restrictions.

use gpudog_core::protocol::{is_valid_gpu_uuid, same_uuid};

pub fn render(uuids: &[String]) -> String {
    uuids.join(",")
}

/// Parses UUID lists only; not an interpreter for CUDA_VISIBLE_DEVICES.
/// CUDA itself resolves environment restrictions in `VisibleDevices::probe`.
pub fn parse_uuid_list(text: &[u8]) -> Option<Vec<String>> {
    let text = std::str::from_utf8(text).ok()?.trim();
    if text.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    for entry in text.split(',').map(str::trim) {
        if !is_valid_gpu_uuid(entry) {
            return None;
        }
        if !out.iter().any(|seen: &String| same_uuid(seen, entry)) {
            out.push(entry.to_owned());
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_uuid_order() {
        assert_eq!(render(&["GPU-b".into(), "GPU-a".into()]), "GPU-b,GPU-a");
    }

    #[test]
    fn parses_only_uuid_lists_and_deduplicates() {
        assert_eq!(
            parse_uuid_list(b"GPU-a,gpu-A,GPU-b"),
            Some(vec!["GPU-a".into(), "GPU-b".into()])
        );
        for text in [b"".as_slice(), b"0,1", b"GPU-a,", b"MIG-a", b"\xff"] {
            assert_eq!(parse_uuid_list(text), None);
        }
    }
}
