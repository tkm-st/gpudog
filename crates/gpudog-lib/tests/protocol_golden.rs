//! Protocol-v6 compatibility against the independent Python encoder.
//! Historical v3-v5 files are retained; older peers fail the handshake.

use gpudog_lib::protocol::*;

fn parse_hex(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex"))
        .collect()
}

fn golden() -> Vec<(&'static str, Vec<u8>)> {
    include_str!("golden/protocol_v6.txt")
        .lines()
        .map(|line| {
            let (name, hex) = line.split_once(' ').expect("`name hex` line");
            (name, parse_hex(hex))
        })
        .collect()
}

/// Independently generated malformed v6 frames with numeric error categories.
fn malformed() -> Vec<(&'static str, Vec<u8>, ProtocolError)> {
    include_str!("golden/protocol_v6_malformed.txt")
        .lines()
        .map(|line| {
            let mut fields = line.split(' ');
            let (name, hex, code) = (
                fields.next().unwrap(),
                fields.next().unwrap(),
                fields.next().unwrap(),
            );
            let error = match code {
                "1" => ProtocolError::Truncated,
                "2" => ProtocolError::FrameTooLarge,
                "3" => ProtocolError::UnknownMessageType,
                "4" => ProtocolError::TrailingBytes,
                "5" => ProtocolError::ValueOutOfRange,
                "6" => ProtocolError::InvalidMessage,
                other => panic!("unknown ProtocolError category {other} for {name}"),
            };
            (name, parse_hex(hex), error)
        })
        .collect()
}

#[test]
fn decode_rejects_malformed_frames_with_the_expected_error() {
    let cases = malformed();
    assert!(cases.len() >= 32);
    for (name, bytes, error) in cases {
        assert_eq!(decode_frame(&bytes), Err(error), "{name}");
    }
}

const U0: &str = "GPU-8f6a1c2d-0000-1111-2222-333344445555";
const U1: &str = "GPU-1C2D3E4F-aaaa-bbbb-cccc-ddddeeeeffff";
const U2: &str = "GPU-static-3";

fn expected(name: &str) -> Message {
    match name {
        "hello" => Hello::default().into(),
        "hello_ack" => HelloAck::default().into(),
        "acquire_count" => Acquire {
            selection: GpuSelection::Count,
            num_gpus: 4,
            gpus: vec![],
            uuids: vec![],
            candidates: vec![U0.into(), U1.into(), U2.into(), "GPU-static-7".into()],
            timeout_ms: 30_000,
            command: b"python train.py --epochs 10".to_vec(),
        }
        .into(),
        "acquire_uuids" => Acquire {
            selection: GpuSelection::ExplicitUuids,
            num_gpus: 2,
            gpus: vec![],
            uuids: vec![U0.into(), U1.into()],
            candidates: vec![],
            timeout_ms: 0,
            command: b"train".to_vec(),
        }
        .into(),
        "acquire_candidates" => Acquire {
            candidates: vec![U1.into(), U2.into()],
            ..Acquire::default()
        }
        .into(),
        "queued" => Queued { position: 7 }.into(),
        "granted" => Granted {
            gpus: vec![0, 3, 1023],
            uuids: vec![U0.into(), U1.into(), U2.into()],
        }
        .into(),
        "denied" => Denied {
            reason: DenyReason::TIMED_OUT,
            detail: b"no device freed".to_vec(),
        }
        .into(),
        "ping" => Message::Ping,
        "pong" => Message::Pong,
        "status" => Message::Status,
        "status_reply_empty" => StatusReply::default().into(),
        "status_reply_full" => StatusReply {
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
                    name: vec![],
                    uuid: vec![],
                },
            ],
            holders: vec![ReservationInfo {
                id: 9,
                uid: 1000,
                pid: -1,
                gpus: vec![0, 1],
                num_gpus: 2,
                age_ms: 123_456,
                position: 0,
                command: b"python train.py".to_vec(),
            }],
            waiting: vec![ReservationInfo {
                id: 10,
                uid: 1001,
                pid: 7,
                gpus: vec![],
                num_gpus: 3,
                age_ms: 0,
                position: 2,
                command: vec![],
            }],
            ..StatusReply::default()
        }
        .into(),
        "status_reply_healthy" => StatusReply {
            observing: true,
            watch_unmanaged_ms: 1,
            observation_age_ms: Some(0),
            ..StatusReply::default()
        }
        .into(),
        "status_reply_failed" => StatusReply {
            observing: true,
            watch_unmanaged_ms: 3_600_000,
            allocation_paused: true,
            observation_error: b"NVML unavailable".to_vec(),
            ..StatusReply::default()
        }
        .into(),
        "status_reply_process_snapshot" => StatusReply {
            device_count: 4,
            observing: true,
            watch_unmanaged_ms: 1000,
            observation_age_ms: Some(42),
            busy_gpus: vec![1, 2],
            processes: vec![
                GpuProcessInfo {
                    gpu: 0,
                    pid: 101,
                    uid: Some(1000),
                    start_time: Some(123456),
                    used_memory: Some(4096),
                    ownership: ProcessOwnership::Managed,
                    command: b"python train.py".to_vec(),
                },
                GpuProcessInfo {
                    gpu: 1,
                    pid: 102,
                    uid: Some(1000),
                    start_time: Some(123457),
                    used_memory: Some(0),
                    ownership: ProcessOwnership::WrongGpu,
                    command: b"wrong GPU".to_vec(),
                },
                GpuProcessInfo {
                    gpu: 2,
                    pid: 103,
                    uid: Some(0),
                    start_time: Some(0),
                    used_memory: None,
                    ownership: ProcessOwnership::NotManaged,
                    command: b"unmanaged".to_vec(),
                },
                GpuProcessInfo {
                    gpu: 3,
                    pid: 104,
                    ..GpuProcessInfo::default()
                },
            ],
            ..StatusReply::default()
        }
        .into(),
        "denied_unknown_reason" => Denied {
            reason: DenyReason(0xEE),
            detail: b"future".to_vec(),
        }
        .into(),
        other => panic!("golden file has an entry this test does not know: {other}"),
    }
}

#[test]
fn every_message_type_has_a_golden_frame() {
    let names: Vec<_> = golden().into_iter().map(|(name, _)| name).collect();
    let mut types: Vec<_> = names
        .iter()
        .map(|name| expected(name).message_type() as u16)
        .collect();
    types.sort_unstable();
    types.dedup();
    assert_eq!(types, (1..=10).collect::<Vec<u16>>());
}

#[test]
fn encode_matches_the_independent_v6_bytes() {
    for (name, bytes) in golden() {
        assert_eq!(encode(&expected(name)).unwrap(), bytes, "{name}");
    }
}

#[test]
fn decode_accepts_the_independent_v6_bytes() {
    for (name, bytes) in golden() {
        assert_eq!(decode_frame(&bytes).unwrap(), expected(name), "{name}");
    }
}
