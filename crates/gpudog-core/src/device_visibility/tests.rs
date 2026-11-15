use super::*;

fn view() -> VisibleDevices {
    VisibleDevices::from_uuids(vec!["GPU-b".into(), "GPU-a".into(), "GPU-c".into()]).unwrap()
}

#[test]
fn local_ordinals_resolve_in_cuda_order_not_daemon_order() {
    let parsed = Acquire {
        selection: GpuSelection::ExplicitList,
        num_gpus: 2,
        gpus: vec![1, 0],
        ..Acquire::default()
    };
    let resolved = view().prepare_acquire(&parsed).unwrap();
    assert_eq!(resolved.selection, GpuSelection::ExplicitUuids);
    assert_eq!(resolved.uuids, ["GPU-a", "GPU-b"]);
    assert!(resolved.gpus.is_empty());
}

#[test]
fn count_always_carries_the_actual_view_and_never_unrestricts_empty() {
    let resolved = view().prepare_acquire(&Acquire::default()).unwrap();
    assert_eq!(resolved.candidates, ["GPU-b", "GPU-a", "GPU-c"]);
    assert!(VisibleDevices::from_uuids(vec![])
        .unwrap()
        .prepare_acquire(&Acquire::default())
        .is_err());
}

#[test]
fn explicit_uuid_and_index_outside_view_are_rejected() {
    let by_uuid = Acquire {
        selection: GpuSelection::ExplicitUuids,
        uuids: vec!["GPU-other".into()],
        ..Acquire::default()
    };
    assert!(view().prepare_acquire(&by_uuid).is_err());
    let by_index = Acquire {
        selection: GpuSelection::ExplicitList,
        gpus: vec![3],
        ..Acquire::default()
    };
    assert!(view().prepare_acquire(&by_index).is_err());
}

#[test]
fn count_respects_additional_candidates_and_fails_insufficient() {
    let request = Acquire {
        candidates: vec!["GPU-A".into(), "GPU-other".into()],
        ..Acquire::default()
    };
    assert_eq!(
        view().prepare_acquire(&request).unwrap().candidates,
        ["GPU-a"]
    );
    assert!(view()
        .prepare_acquire(&Acquire {
            num_gpus: 2,
            ..request
        })
        .is_err());
}

#[test]
fn malformed_duplicate_and_oversized_views_are_rejected() {
    for uuids in [
        vec!["MIG-a".into()],
        vec!["GPU-a".into(), "gpu-A".into()],
        vec!["GPU-\x1b".into()],
    ] {
        assert!(VisibleDevices::from_uuids(uuids).is_err());
    }
    assert!(VisibleDevices::from_uuids(vec!["GPU-a".into(); MAX_GPU_COUNT as usize + 1]).is_err());
}

#[test]
fn bad_count_and_inconsistent_raw_requests_fail_before_io() {
    for request in [
        Acquire {
            num_gpus: 0,
            ..Acquire::default()
        },
        Acquire {
            gpus: vec![0],
            ..Acquire::default()
        },
        Acquire {
            selection: GpuSelection::ExplicitList,
            num_gpus: 2,
            gpus: vec![0],
            ..Acquire::default()
        },
    ] {
        assert!(view().prepare_acquire(&request).is_err());
    }
}

#[test]
fn grants_must_match_view_request_count_and_order() {
    let request = view()
        .prepare_acquire(&Acquire {
            candidates: vec!["GPU-a".into()],
            ..Acquire::default()
        })
        .unwrap();
    let grant = |uuid: &str| Granted {
        gpus: vec![0],
        uuids: vec![uuid.into()],
        ..Granted::default()
    };
    assert!(view().validate_grant(&request, &grant("gpu-A")).is_ok());
    assert!(view().validate_grant(&request, &grant("GPU-b")).is_err());
    assert!(view()
        .validate_grant(&request, &Granted::default())
        .is_err());
    let explicit = view()
        .prepare_acquire(&Acquire {
            selection: GpuSelection::ExplicitList,
            num_gpus: 2,
            gpus: vec![0, 1],
            ..Acquire::default()
        })
        .unwrap();
    assert!(view()
        .validate_grant(
            &explicit,
            &Granted {
                gpus: vec![0, 1],
                uuids: vec!["GPU-a".into(), "GPU-b".into()],
                ..Granted::default()
            }
        )
        .is_err());
}

#[test]
fn only_cudarc_source_files_count_as_cudarc_panics() {
    for file in [
        "/usr/local/cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cudarc-0.19.10/src/lib.rs",
        "/home/u/.cargo/registry/src/index.crates.io-x/cudarc-0.19.10/src/driver/sys/mod.rs",
        "vendor/cudarc/src/lib.rs",
        r"C:\cargo\registry\src\x\cudarc-0.19.10\src\lib.rs",
    ] {
        assert!(is_cudarc_source(file), "{file}");
    }
    for file in [
        "crates/gpudog-core/src/device_visibility.rs",
        "/registry/src/x/notcudarc-1.0.0/src/lib.rs",
        "/registry/src/x/cudarc_extra/src/lib.rs",
        "",
    ] {
        assert!(!is_cudarc_source(file), "{file}");
    }
}

#[test]
fn panics_outside_cudarc_are_still_caught() {
    let caught = catch_cudarc_panic(|| -> u8 { panic!("not from cudarc") });
    assert!(caught.is_err());
    assert_eq!(catch_cudarc_panic(|| 7).unwrap(), 7);
}

#[test]
fn only_cudarc_loader_messages_are_quiet() {
    assert!(is_cudarc_loader_failure(
        "Unable to dynamically load the \"cuda\" shared library - searched for library names: []"
    ));
    assert!(is_cudarc_loader_failure(
        "Missing symbol cuDeviceGetUuid: dlsym failed"
    ));
    assert!(!is_cudarc_loader_failure(
        "Unsupported target pointer width"
    ));
    assert!(!is_cudarc_loader_failure("index out of bounds"));
}
