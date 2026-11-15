use super::*;

fn frame_of(msg: impl Into<Message>) -> Vec<u8> {
    encode(&msg.into()).expect("encode")
}

fn check_round_trip(msg: impl Into<Message>) {
    let msg = msg.into();
    let frame = frame_of(msg.clone());
    assert!(frame.len() >= LENGTH_PREFIX_SIZE);
    assert_eq!(
        decode_body_size(&frame).unwrap() as usize,
        frame.len() - LENGTH_PREFIX_SIZE
    );
    assert_eq!(decode_frame(&frame).unwrap(), msg);
}

/// Build a frame by hand so decoding can be tested against bytes that
/// `encode` would never produce.
fn make_frame(body: &[u8]) -> Vec<u8> {
    let mut frame = (body.len() as u32).to_le_bytes().to_vec();
    frame.extend_from_slice(body);
    frame
}

fn decode_err(body: &[u8]) -> ProtocolError {
    decode_frame(&make_frame(body)).unwrap_err()
}

fn encode_err(msg: impl Into<Message>) -> ProtocolError {
    encode(&msg.into()).unwrap_err()
}

fn granted(gpus: &[u32]) -> Granted {
    Granted {
        gpus: gpus.to_vec(),
        uuids: gpus.iter().map(|i| format!("GPU-{i}")).collect(),
        hold_limit_ms: 0,
    }
}

fn uuids(list: &[&str]) -> Vec<String> {
    list.iter().map(|&u| u.to_owned()).collect()
}

fn acquire(selection: GpuSelection, num_gpus: u32, gpus: &[u32]) -> Acquire {
    Acquire {
        selection,
        num_gpus,
        gpus: gpus.to_vec(),
        candidates: if selection == GpuSelection::Count {
            uuids(&["GPU-a"])
        } else {
            vec![]
        },
        ..Acquire::default()
    }
}

/// An `acquire` body with the given selection, `num_gpus` and timeout, no
/// GPU or UUID list and an empty command.
fn acquire_body(selection: u8, num_gpus: u32, timeout_ms: u64) -> Vec<u8> {
    let mut body = vec![0x03, 0x00, selection];
    body.extend_from_slice(&num_gpus.to_le_bytes());
    body.extend_from_slice(&[0; 4]); // gpu count
    body.extend_from_slice(&[0; 4]); // uuid count
    body.extend_from_slice(&[0; 4]); // candidate count
    body.extend_from_slice(&timeout_ms.to_le_bytes());
    body.extend_from_slice(&[0; 4]); // command
    body
}

#[test]
fn round_trips_every_message_type() {
    check_round_trip(Message::Pong);
    check_round_trip(HelloAck::default());
    check_round_trip(Hello::default());
    check_round_trip(Message::Ping);
    check_round_trip(Queued { position: 7 });
    check_round_trip(granted(&[0, 3, 1023]));
    check_round_trip(Denied {
        reason: DenyReason::TIMED_OUT,
        detail: b"no device freed".to_vec(),
    });
    check_round_trip(Denied::default());
    check_round_trip(Acquire {
        num_gpus: 4,
        timeout_ms: 30_000,
        candidates: uuids(&["GPU-a"]),
        ..Acquire::default()
    });
    check_round_trip(Acquire {
        selection: GpuSelection::ExplicitUuids,
        num_gpus: 2,
        uuids: uuids(&["GPU-2", "GPU-5"]),
        ..Acquire::default()
    });
    check_round_trip(Granted {
        gpus: vec![0, 3, 1023],
        uuids: uuids(&["GPU-a", "GPU-B-1", "gpu-0c"]),
        hold_limit_ms: 7_200_000,
    });
    check_round_trip(Acquire {
        selection: GpuSelection::ExplicitUuids,
        num_gpus: 2,
        uuids: uuids(&["GPU-8f6a", "GPU-1c2d"]),
        ..Acquire::default()
    });
}

#[test]
fn accepts_values_at_the_documented_bounds() {
    check_round_trip(Granted {
        gpus: vec![MAX_GPU_COUNT - 1],
        uuids: vec![format!("GPU-{}", "f".repeat(MAX_UUID_SIZE as usize - 4))],
        hold_limit_ms: MAX_HOLD_LIMIT_MS,
    });
    check_round_trip(granted(&(0..MAX_GPU_COUNT).collect::<Vec<_>>()));
    check_round_trip(Acquire {
        num_gpus: MAX_GPU_COUNT,
        timeout_ms: MAX_TIMEOUT_MS,
        candidates: uuids(&["GPU-a"]),
        ..Acquire::default()
    });
    check_round_trip(Denied {
        reason: DenyReason::INTERNAL_ERROR,
        detail: vec![b'x'; MAX_STRING_SIZE as usize],
    });
}

#[test]
fn accepts_a_body_of_exactly_max_body_size() {
    assert_eq!(
        decode_body_size(&MAX_BODY_SIZE.to_le_bytes()),
        Ok(MAX_BODY_SIZE)
    );
    assert_eq!(
        decode_body_size(&(MAX_BODY_SIZE + 1).to_le_bytes()),
        Err(ProtocolError::FrameTooLarge)
    );
}

#[test]
fn rejects_a_body_larger_than_max_body_size_given_to_decode_body() {
    let body = vec![0; MAX_BODY_SIZE as usize + 1];
    assert_eq!(decode_body(&body), Err(ProtocolError::FrameTooLarge));
}

#[test]
fn rejects_an_acquire_truncated_mid_field() {
    let full = frame_of(Acquire {
        num_gpus: 2,
        timeout_ms: 1234,
        candidates: uuids(&["GPU-a"]),
        ..Acquire::default()
    });
    let body = &full[LENGTH_PREFIX_SIZE..];
    for keep in 1..body.len() {
        assert_eq!(
            decode_body(&body[..keep]),
            Err(ProtocolError::Truncated),
            "keep = {keep}"
        );
    }
}

#[test]
fn rejects_a_denied_string_shorter_than_its_declared_length() {
    let body = [
        0x06, 0x00, 0x01, 0x00, 0x0A, 0x00, 0x00, 0x00, b'a', b'b', b'c',
    ];
    assert_eq!(decode_err(&body), ProtocolError::Truncated);
}

#[test]
fn rejects_an_oversized_length_prefix() {
    let frame = [0xFF; 4];
    assert_eq!(decode_body_size(&frame), Err(ProtocolError::FrameTooLarge));
    assert_eq!(decode_frame(&frame), Err(ProtocolError::FrameTooLarge));
}

#[test]
fn rejects_a_short_length_prefix() {
    assert_eq!(
        decode_body_size(&[0x01, 0x00]),
        Err(ProtocolError::Truncated)
    );
}

#[test]
fn rejects_a_body_shorter_than_its_length_prefix_claims() {
    let mut frame = frame_of(Message::Pong);
    frame.pop();
    assert_eq!(decode_frame(&frame), Err(ProtocolError::Truncated));
}

#[test]
fn rejects_an_empty_body() {
    assert_eq!(decode_err(&[]), ProtocolError::Truncated);
}

#[test]
fn rejects_an_unknown_message_type() {
    assert_eq!(decode_err(&[0xEE, 0x00]), ProtocolError::UnknownMessageType);
    assert!(!is_known_message_type(0));
    assert!(is_known_message_type(MessageType::Hello as u16));
}

#[test]
fn rejects_trailing_bytes_after_a_complete_message() {
    assert_eq!(
        decode_err(&[0x07, 0x00, 0x99]),
        ProtocolError::TrailingBytes
    );
}

#[test]
fn rejects_a_gpu_count_that_cannot_fit_in_the_body() {
    assert_eq!(
        decode_err(&[0x05, 0x00, 0x00, 0x04, 0x00, 0x00]),
        ProtocolError::Truncated
    );
}

#[test]
fn rejects_a_gpu_count_above_the_documented_bound() {
    assert_eq!(
        decode_err(&[0x05, 0x00, 0x01, 0x04, 0x00, 0x00]),
        ProtocolError::ValueOutOfRange
    );
}

#[test]
fn rejects_an_out_of_range_gpu_index() {
    let body = [0x05, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00];
    assert_eq!(decode_err(&body), ProtocolError::ValueOutOfRange);
}

#[test]
fn rejects_an_oversized_string_length() {
    let body = [0x06, 0x00, 0x01, 0x00, 0x01, 0x10, 0x00, 0x00];
    assert_eq!(decode_err(&body), ProtocolError::ValueOutOfRange);
}

#[test]
fn rejects_an_inconsistent_acquire_request() {
    assert_eq!(
        encode_err(acquire(GpuSelection::Count, 1, &[0])),
        ProtocolError::InvalidMessage
    );
    assert_eq!(
        encode_err(acquire(GpuSelection::ExplicitList, 1, &[])),
        ProtocolError::InvalidMessage
    );
    assert_eq!(
        encode_err(acquire(GpuSelection::Count, 0, &[])),
        ProtocolError::ValueOutOfRange
    );
    assert_eq!(
        decode_err(&acquire_body(0x7F, 1, 0)),
        ProtocolError::InvalidMessage
    );
}

#[test]
fn rejects_an_empty_granted_message() {
    assert_eq!(
        encode_err(Granted::default()),
        ProtocolError::ValueOutOfRange
    );
    assert_eq!(
        decode_err(&[
            0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
            0, 0, 0, 0, 0, 0, 0, 0, // hold limit
        ]),
        ProtocolError::ValueOutOfRange
    );
}

#[test]
fn rejects_a_hold_limit_above_the_documented_bound() {
    let over = Granted {
        hold_limit_ms: MAX_HOLD_LIMIT_MS + 1,
        ..granted(&[0])
    };
    assert_eq!(encode_err(over), ProtocolError::ValueOutOfRange);
    let mut reply = StatusReply {
        holders: vec![ReservationInfo {
            hold_limit_ms: MAX_HOLD_LIMIT_MS + 1,
            ..ReservationInfo::default()
        }],
        ..StatusReply::default()
    };
    assert_eq!(encode_err(reply.clone()), ProtocolError::ValueOutOfRange);
    reply.holders[0].hold_limit_ms = MAX_HOLD_LIMIT_MS;
    check_round_trip(reply);
}

#[test]
fn rejects_duplicate_gpu_indices() {
    assert_eq!(encode_err(granted(&[3, 3])), ProtocolError::InvalidMessage);
    assert_eq!(
        encode_err(acquire(GpuSelection::ExplicitList, 2, &[2, 2])),
        ProtocolError::InvalidMessage
    );
    let body = [
        0x05, 0x00, 0x02, 0x00, 0x00, 0x00, 0x07, 0x00, 0x00, 0x00, 0x07, 0x00, 0x00,
        0x00, // gpus
        0x02, 0x00, 0x00, 0x00, // two uuids
        0x05, 0x00, 0x00, 0x00, b'G', b'P', b'U', b'-', b'a', //
        0x05, 0x00, 0x00, 0x00, b'G', b'P', b'U', b'-', b'b', //
        0, 0, 0, 0, 0, 0, 0, 0, // hold limit
    ];
    assert_eq!(decode_err(&body), ProtocolError::InvalidMessage);
}

#[test]
fn rejects_a_num_gpus_above_the_documented_bound() {
    assert_eq!(
        encode_err(acquire(GpuSelection::Count, MAX_GPU_COUNT + 1, &[])),
        ProtocolError::ValueOutOfRange
    );
    assert_eq!(
        decode_err(&acquire_body(0, u32::MAX, 0)),
        ProtocolError::ValueOutOfRange
    );
}

#[test]
fn requires_num_gpus_to_match_an_explicit_gpu_list() {
    assert_eq!(
        encode_err(acquire(GpuSelection::ExplicitList, 99, &[1])),
        ProtocolError::InvalidMessage
    );
}

#[test]
fn rejects_a_timeout_that_would_overflow_the_daemons_clock() {
    let over = Acquire {
        timeout_ms: MAX_TIMEOUT_MS + 1,
        ..Acquire::default()
    };
    assert_eq!(encode_err(over), ProtocolError::ValueOutOfRange);
    assert_eq!(
        decode_err(&acquire_body(0, 1, 0x00FF_FFFF_FFF2_0000)),
        ProtocolError::ValueOutOfRange
    );
    check_round_trip(Acquire {
        timeout_ms: MAX_TIMEOUT_MS,
        candidates: uuids(&["GPU-a"]),
        ..Acquire::default()
    });
}

#[test]
fn rejects_an_oversized_denied_detail() {
    let denied = Denied {
        reason: DenyReason::INTERNAL_ERROR,
        detail: vec![b'x'; MAX_STRING_SIZE as usize + 1],
    };
    assert_eq!(encode_err(denied), ProtocolError::ValueOutOfRange);
}

#[test]
fn accepts_an_unknown_deny_reason() {
    let decoded = decode_frame(&make_frame(&[
        0x06, 0x00, 0xEE, 0x00, 0x00, 0x00, 0x00, 0x00,
    ]))
    .unwrap();
    assert_eq!(
        decoded,
        Message::Denied(Denied {
            reason: DenyReason(0xEE),
            detail: vec![],
        })
    );
}

#[test]
fn message_type_matches_the_encoded_discriminator() {
    let frame = frame_of(Queued { position: 1 });
    assert_eq!(frame[LENGTH_PREFIX_SIZE], MessageType::Queued as u8);
    assert_eq!(
        Message::from(Queued::default()).message_type(),
        MessageType::Queued
    );
}

#[test]
fn round_trips_a_status_request_and_an_empty_reply() {
    check_round_trip(Message::Status);
    check_round_trip(StatusReply::default());
}

fn populated_status_reply() -> StatusReply {
    StatusReply {
        device_count: 4,
        truncated: true,
        devices: vec![
            DeviceInfo {
                index: 0,
                name: b"Test GPU".to_vec(),
                uuid: b"GPU-abcdef".to_vec(),
            },
            DeviceInfo {
                index: 3,
                ..DeviceInfo::default()
            },
        ],
        holders: vec![ReservationInfo {
            id: 9,
            uid: 1000,
            pid: 4242,
            gpus: vec![0, 1],
            num_gpus: 2,
            age_ms: 123_456,
            hold_limit_ms: 3_600_000,
            position: 0,
            command: b"python train.py".to_vec(),
        }],
        waiting: vec![ReservationInfo {
            id: 10,
            uid: 1001,
            pid: 7,
            num_gpus: 3,
            position: 2,
            ..ReservationInfo::default()
        }],
        ..StatusReply::default()
    }
}

#[test]
fn round_trips_a_fully_populated_status_reply() {
    check_round_trip(populated_status_reply());
}

fn process_snapshot() -> StatusReply {
    StatusReply {
        device_count: 4,
        observing: true,
        watch_unmanaged_ms: 1000,
        observation_age_ms: Some(0),
        busy_gpus: vec![1, 2],
        processes: vec![GpuProcessInfo::default()],
        ..StatusReply::default()
    }
}

#[test]
fn process_ownership_has_stable_wire_values_labels_and_default() {
    for (ownership, value, label) in [
        (ProcessOwnership::Managed, 0, "managed"),
        (ProcessOwnership::WrongGpu, 1, "managed (wrong GPU)"),
        (ProcessOwnership::NotManaged, 2, "not managed"),
        (ProcessOwnership::Unknown, 3, "unknown"),
    ] {
        assert_eq!(ownership as u8, value);
        assert_eq!(ownership.label(), label);
    }
    assert_eq!(
        GpuProcessInfo::default().ownership,
        ProcessOwnership::Unknown
    );
}

#[test]
fn round_trips_process_snapshots_with_all_option_combinations() {
    for ownership in [
        ProcessOwnership::Managed,
        ProcessOwnership::WrongGpu,
        ProcessOwnership::NotManaged,
        ProcessOwnership::Unknown,
    ] {
        for mask in 0..16 {
            let mut reply = process_snapshot();
            reply.observation_age_ms = (mask & 1 != 0).then_some(u64::MAX);
            reply.processes[0] = GpuProcessInfo {
                gpu: 3,
                pid: u32::MAX,
                uid: (mask & 2 != 0).then_some(0),
                start_time: (mask & 4 != 0).then_some(u64::MAX),
                used_memory: (mask & 8 != 0).then_some(0),
                command: vec![0xFF, b'\n'],
                ownership,
            };
            check_round_trip(reply);
        }
    }
    check_round_trip(StatusReply {
        observing: true,
        watch_unmanaged_ms: 3_600_000,
        allocation_paused: true,
        observation_error: b"NVML unavailable".to_vec(),
        ..StatusReply::default()
    });
}

#[test]
fn process_snapshot_bounds_are_enforced_on_encode() {
    let mut reply = process_snapshot();
    for interval in [0, 3_600_001, u64::MAX] {
        reply.watch_unmanaged_ms = interval;
        assert_eq!(encode_err(reply.clone()), ProtocolError::ValueOutOfRange);
    }
    reply.watch_unmanaged_ms = 1;
    reply.observation_error = vec![b'x'; MAX_COMMAND_SIZE as usize];
    reply.processes[0].command = reply.observation_error.clone();
    check_round_trip(reply.clone());
    reply.observation_error.push(b'x');
    assert_eq!(encode_err(reply.clone()), ProtocolError::ValueOutOfRange);
    reply.observation_error.clear();
    reply.processes[0].command.push(b'x');
    assert_eq!(encode_err(reply.clone()), ProtocolError::ValueOutOfRange);
    reply.processes[0].command.clear();
    for gpu in [reply.device_count, MAX_GPU_COUNT, u32::MAX] {
        reply.processes[0].gpu = gpu;
        assert_eq!(encode_err(reply.clone()), ProtocolError::ValueOutOfRange);
        reply.processes[0].gpu = 0;
        reply.busy_gpus = vec![gpu];
        assert_eq!(encode_err(reply.clone()), ProtocolError::ValueOutOfRange);
    }
    reply.busy_gpus = vec![1, 1];
    assert_eq!(encode_err(reply.clone()), ProtocolError::InvalidMessage);
    reply.busy_gpus.clear();
    reply.processes = vec![GpuProcessInfo::default(); MAX_STATUS_ENTRIES as usize];
    check_round_trip(reply.clone());
    reply.processes.push(GpuProcessInfo::default());
    assert_eq!(encode_err(reply), ProtocolError::ValueOutOfRange);
}

#[test]
fn process_snapshots_respect_frame_limits_and_truncated_device_tables() {
    let mut reply = StatusReply {
        device_count: MAX_GPU_COUNT,
        truncated: true,
        busy_gpus: (0..MAX_GPU_COUNT).collect(),
        processes: vec![GpuProcessInfo {
            gpu: MAX_GPU_COUNT - 1,
            ..GpuProcessInfo::default()
        }],
        observation_error: vec![0xFF, 0, b'\n'],
        ..StatusReply::default()
    };
    // An omitted device table does not invalidate an otherwise valid GPU.
    check_round_trip(reply.clone());
    // device_count is informational and has no independent protocol cap.
    reply.device_count = u32::MAX;
    check_round_trip(reply.clone());
    reply.busy_gpus.clear();
    reply.processes = vec![
        GpuProcessInfo {
            command: vec![b'x'; MAX_COMMAND_SIZE as usize],
            ..GpuProcessInfo::default()
        };
        MAX_STATUS_ENTRIES as usize
    ];
    // Individually valid rows still have to fit the overall frame bound.
    assert_eq!(encode_err(reply), ProtocolError::FrameTooLarge);
}

/// Empty lists and absent options keep offsets explicit for malformed tests.
fn minimal_process_body() -> Vec<u8> {
    let reply = StatusReply {
        device_count: 1,
        processes: vec![GpuProcessInfo::default()],
        ..StatusReply::default()
    };
    frame_of(reply)[LENGTH_PREFIX_SIZE..].to_vec()
}

#[test]
fn refuses_noncanonical_bools_options_and_process_ownership() {
    // truncated, observing, paused, age-present, uid/start/memory-present,
    // ownership: each has an independent discriminator.
    for offset in [18, 19, 28, 29, 50, 51, 52, 53] {
        for invalid in [4, 255] {
            let mut body = minimal_process_body();
            body[offset] = invalid;
            assert_eq!(decode_err(&body), ProtocolError::InvalidMessage, "{offset}");
        }
    }
    for offset in [18, 19, 28, 29, 50, 51, 52] {
        let mut body = minimal_process_body();
        body[offset] = 2;
        assert_eq!(decode_err(&body), ProtocolError::InvalidMessage, "{offset}");
    }
}

#[test]
fn new_status_lengths_and_gpu_bounds_are_checked_on_decode() {
    for (offset, value) in [(30, 257), (34, 1025), (38, 257), (42, 1024), (54, 257)] {
        let mut body = minimal_process_body();
        body[offset..offset + 4].copy_from_slice(&u32::to_le_bytes(value));
        assert_eq!(
            decode_err(&body),
            ProtocolError::ValueOutOfRange,
            "{offset}"
        );
    }
    let mut body = minimal_process_body();
    body[42..46].copy_from_slice(&1_u32.to_le_bytes());
    assert_eq!(decode_err(&body), ProtocolError::ValueOutOfRange);
    body[19] = 1;
    body[42..46].copy_from_slice(&0_u32.to_le_bytes());
    assert_eq!(decode_err(&body), ProtocolError::ValueOutOfRange);
    body[20..28].copy_from_slice(&3_600_001_u64.to_le_bytes());
    assert_eq!(decode_err(&body), ProtocolError::ValueOutOfRange);
}

#[test]
fn every_process_snapshot_prefix_is_truncated_and_trailing_bytes_are_refused() {
    let mut reply = process_snapshot();
    reply.processes[0].uid = Some(1000);
    reply.processes[0].start_time = Some(12345);
    reply.processes[0].used_memory = Some(1024);
    reply.processes[0].command = b"train".to_vec();
    let mut body = frame_of(reply)[LENGTH_PREFIX_SIZE..].to_vec();
    for end in 0..body.len() {
        assert_eq!(decode_err(&body[..end]), ProtocolError::Truncated, "{end}");
    }
    body.push(0);
    assert_eq!(decode_err(&body), ProtocolError::TrailingBytes);
}

#[test]
fn round_trips_a_negative_pid_in_a_status_reply() {
    check_round_trip(StatusReply {
        holders: vec![ReservationInfo {
            id: 1,
            pid: -1,
            num_gpus: 1,
            command: b"x".to_vec(),
            ..ReservationInfo::default()
        }],
        ..StatusReply::default()
    });
}

#[test]
fn round_trips_a_command_on_an_acquire_request() {
    check_round_trip(Acquire {
        command: b"python train.py --epochs 10".to_vec(),
        candidates: uuids(&["GPU-a"]),
        ..Acquire::default()
    });
}

#[test]
fn round_trips_a_command_that_is_not_utf8() {
    // Wire strings are bytes; a hostile client's invalid UTF-8 must survive
    // decoding unchanged so length bounds hold for the daemon's echo.
    check_round_trip(Acquire {
        command: vec![0xFF, 0xFE, b'a'],
        candidates: uuids(&["GPU-a"]),
        ..Acquire::default()
    });
}

#[test]
fn refuses_a_command_longer_than_the_documented_bound() {
    let mut request = Acquire {
        command: vec![b'x'; MAX_COMMAND_SIZE as usize],
        candidates: uuids(&["GPU-a"]),
        ..Acquire::default()
    };
    assert!(encode(&request.clone().into()).is_ok());
    request.command.push(b'x');
    assert_eq!(encode_err(request), ProtocolError::ValueOutOfRange);
}

#[test]
fn refuses_a_status_reply_whose_command_exceeds_the_bound() {
    let reply = StatusReply {
        holders: vec![ReservationInfo {
            id: 1,
            num_gpus: 1,
            command: vec![b'x'; MAX_COMMAND_SIZE as usize + 1],
            ..ReservationInfo::default()
        }],
        ..StatusReply::default()
    };
    assert_eq!(encode_err(reply), ProtocolError::ValueOutOfRange);
}

#[test]
fn refuses_a_status_reply_with_too_many_entries() {
    let reply = StatusReply {
        waiting: (0..=u64::from(MAX_STATUS_ENTRIES))
            .map(|id| ReservationInfo {
                id,
                num_gpus: 1,
                ..ReservationInfo::default()
            })
            .collect(),
        ..StatusReply::default()
    };
    assert_eq!(encode_err(reply), ProtocolError::ValueOutOfRange);
}

#[test]
fn refuses_a_status_reply_naming_an_impossible_device() {
    let holder = |gpus: Vec<u32>| StatusReply {
        holders: vec![ReservationInfo {
            id: 1,
            num_gpus: gpus.len() as u32,
            gpus,
            ..ReservationInfo::default()
        }],
        ..StatusReply::default()
    };
    assert_eq!(
        encode_err(holder(vec![MAX_GPU_COUNT])),
        ProtocolError::ValueOutOfRange
    );
    assert_eq!(
        encode_err(holder(vec![2, 2])),
        ProtocolError::InvalidMessage
    );
}

#[test]
fn a_status_reply_with_an_absurd_entry_count_is_refused_before_allocating() {
    let body = [0x0A, 0x00, 0, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF];
    assert_eq!(decode_err(&body), ProtocolError::ValueOutOfRange);
}

#[test]
fn a_status_reply_truncated_mid_entry_is_refused() {
    let body = [0x0A, 0x00, 0, 0, 0, 0, 0x01, 0, 0, 0, 0, 0, 0, 0];
    assert_eq!(decode_err(&body), ProtocolError::Truncated);
}

#[test]
fn the_protocol_version_was_bumped_when_the_hold_limit_joined_the_wire() {
    // v6 removed the priority from Acquire and ReservationInfo; v7 added the
    // hold limit to Granted and ReservationInfo. Older peers must be refused
    // at the handshake rather than left to misparse the new layout.
    assert_eq!(PROTOCOL_VERSION, 7);
    assert!(is_known_message_type(MessageType::Status as u16));
    assert!(is_known_message_type(MessageType::StatusReply as u16));
    assert!(!is_known_message_type(11));
}

#[test]
fn display_text_is_reduced_to_printable_ascii() {
    assert_eq!(sanitize_display_text(b"plain text"), "plain text");
    assert_eq!(sanitize_display_text(b"a\nb"), "a?b");
    assert_eq!(sanitize_display_text(b"\x1b[31m"), "?[31m");
    assert_eq!(sanitize_display_text(b"\x7f"), "?");
    assert_eq!(sanitize_display_text(b"\xff\xfe"), "??");
}

#[test]
fn gpu_uuids_are_validated_by_form() {
    assert!(is_valid_gpu_uuid(
        "GPU-8f6a1c2d-0000-1111-2222-333344445555"
    ));
    assert!(is_valid_gpu_uuid("gpu-ABC"));
    let max = MAX_UUID_SIZE as usize;
    assert!(is_valid_gpu_uuid(&format!("GPU-{}", "a".repeat(max - 4))));
    assert!(!is_valid_gpu_uuid(&format!("GPU-{}", "a".repeat(max - 3))));
    for bad in [
        "GPU-",
        "MIG-abc",
        "0",
        "GPU-a,b",
        "GPU-a b",
        "GPU-a\nb",
        "GPU-a\0b",
        "GPU-\u{e9}",
    ] {
        assert!(!is_valid_gpu_uuid(bad), "{bad:?}");
    }
    assert!(same_uuid("GPU-abc", "gpu-ABC"));
    assert!(!same_uuid("GPU-abc", "GPU-abcd"));
}

#[test]
fn rejects_inconsistent_uuid_fields() {
    let short = Granted {
        gpus: vec![0, 1],
        uuids: uuids(&["GPU-a"]),
        ..Granted::default()
    };
    assert_eq!(encode_err(short), ProtocolError::InvalidMessage);
    let malformed = Granted {
        gpus: vec![0],
        uuids: uuids(&["GPU-a;rm"]),
        ..Granted::default()
    };
    assert_eq!(encode_err(malformed), ProtocolError::InvalidMessage);
    let twice = Acquire {
        selection: GpuSelection::ExplicitUuids,
        num_gpus: 2,
        uuids: uuids(&["GPU-a", "gpu-A"]),
        ..Acquire::default()
    };
    assert_eq!(encode_err(twice), ProtocolError::InvalidMessage);
    let count_with_uuids = Acquire {
        uuids: uuids(&["GPU-a"]),
        ..Acquire::default()
    };
    assert_eq!(encode_err(count_with_uuids), ProtocolError::InvalidMessage);
    let mismatch = Acquire {
        selection: GpuSelection::ExplicitUuids,
        num_gpus: 3,
        uuids: uuids(&["GPU-a"]),
        ..Acquire::default()
    };
    assert_eq!(encode_err(mismatch), ProtocolError::InvalidMessage);
    let list_with_uuids = Acquire {
        uuids: uuids(&["GPU-a"]),
        ..acquire(GpuSelection::ExplicitList, 1, &[0])
    };
    assert_eq!(encode_err(list_with_uuids), ProtocolError::InvalidMessage);
}

#[test]
fn a_non_utf8_uuid_on_the_wire_is_an_invalid_message() {
    let body = [
        0x05, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // gpus [0]
        0x01, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, b'G', b'P', b'U', b'-', 0xFF, //
        0, 0, 0, 0, 0, 0, 0, 0, // hold limit
    ];
    assert_eq!(decode_err(&body), ProtocolError::InvalidMessage);
}

#[test]
fn carries_candidates_only_on_a_count_request() {
    check_round_trip(Acquire {
        candidates: uuids(&["GPU-a", "GPU-b"]),
        ..Acquire::default()
    });
    let explicit = Acquire {
        candidates: uuids(&["GPU-a"]),
        ..acquire(GpuSelection::ExplicitList, 1, &[0])
    };
    assert_eq!(encode_err(explicit), ProtocolError::InvalidMessage);
    let repeated = Acquire {
        candidates: uuids(&["GPU-a", "gpu-A"]),
        ..Acquire::default()
    };
    assert_eq!(encode_err(repeated), ProtocolError::InvalidMessage);
}
