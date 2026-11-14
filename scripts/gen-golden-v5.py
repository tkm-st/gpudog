#!/usr/bin/env python3
"""Generate v5 fixtures independently, retaining the historical v3/v4 bytes.

Usage: python3 scripts/gen-golden-v5.py <existing-output-directory>
"""
from pathlib import Path
import runpy
import sys
import tempfile

root = Path(__file__).resolve().parent.parent
output = Path(sys.argv[1])
(root / "tmp").mkdir(exist_ok=True)
with tempfile.TemporaryDirectory(prefix="golden-v4-", dir=root / "tmp") as scratch:
    sys.argv[1] = scratch
    v4 = runpy.run_path(str(root / "scripts/gen-golden-v4.py"))

v3 = v4["v3"]
u8, u16, u32, u64, s, gl, frame = (
    v3[key] for key in ("u8", "u16", "u32", "u64", "s", "gl", "frame")
)


def option(value, encode):
    return u8(value is not None) + (encode(value) if value is not None else b"")


def process(gpu=0, pid=0, uid=None, start=None, memory=None, ownership=3, command=""):
    return (u32(gpu) + u32(pid) + option(uid, u32) + option(start, u64)
            + option(memory, u64) + u8(ownership) + s(command))


def observation(observing=False, watch_ms=0, paused=False, age=None, error="",
                busy=(), processes=()):
    return (u8(observing) + u64(watch_ms) + u8(paused) + option(age, u64)
            + s(error) + gl(busy) + u32(len(processes)) + b"".join(processes))


def status(device_count=0, **kwargs):
    # No device/reservation entries, not truncated; observation follows.
    return u16(10) + u32(device_count) + u32(0) * 3 + u8(0) + observation(**kwargs)


valid = []
for name, body in v4["valid"]:
    if name in {"hello", "hello_ack"}:
        body = body[:2] + u32(5)
    elif name.startswith("status_reply_"):
        body += observation()
    valid.append((name, body))

snapshot = status(
    4, observing=True, watch_ms=1000, age=42, busy=[1, 2], processes=[
        process(0, 101, 1000, 123456, 4096, 0, "python train.py"),
        process(1, 102, 1000, 123457, 0, 1, "wrong GPU"),
        process(2, 103, 0, 0, None, 2, "unmanaged"),
        process(3, 104),
    ],
)
valid += [
    ("status_reply_healthy", status(observing=True, watch_ms=1, age=0)),
    ("status_reply_failed", status(observing=True, watch_ms=3600000,
                                   paused=True, error="NVML unavailable")),
    ("status_reply_process_snapshot", snapshot),
]

# Preserve non-status malformed cases; existing incomplete status rows still
# fail before the appended layout. Complete legacy status bodies now truncate.
malformed = list(v4["malformed"])
for name, body in v4["valid"]:
    if name.startswith("status_reply_"):
        malformed.append((name + "_legacy_v4", frame(body), 1))

minimal = status(1, processes=[process()])
for name, offset in [("truncated", 18), ("observing", 19), ("paused", 28),
                     ("age", 29), ("uid", 50), ("start", 51), ("memory", 52)]:
    for value in [2, 255]:
        body = minimal[:offset] + u8(value) + minimal[offset + 1:]
        malformed.append((f"status_bad_{name}_{value}", frame(body), 6))
for value in [4, 255]:
    malformed.append((f"status_bad_ownership_{value}",
                      frame(minimal[:53] + u8(value) + minimal[54:]), 6))
for name, offset, value in [("error_length", 30, 257), ("busy_count", 34, 1025),
                            ("process_count", 38, 257), ("process_gpu", 42, 1024),
                            ("process_device_count", 42, 1), ("command_length", 54, 257)]:
    body = minimal[:offset] + u32(value) + minimal[offset + 4:]
    malformed.append((f"status_bad_{name}", frame(body), 5))
for interval in [0, 3600001]:
    malformed.append((f"status_bad_watch_ms_{interval}",
                      frame(status(observing=True, watch_ms=interval)), 5))
malformed += [
    ("status_busy_duplicate", frame(status(2, busy=[1, 1])), 6),
    ("status_busy_device_count", frame(status(2, busy=[2])), 5),
    ("status_busy_gpu_bound", frame(status(2048, busy=[1024])), 5),
    ("status_process_gpu_bound_large_device_count",
     frame(status(2048, processes=[process(1024)])), 5),
    ("status_process_count_unbacked", frame(status()[:-4] + u32(1)), 1),
    ("status_error_unbacked", frame(status()[:30] + u32(3) + b"ab"), 1),
    ("status_command_unbacked", frame(minimal[:54] + u32(3) + b"ab"), 1),
    ("status_trailing_byte", frame(status() + u8(0)), 4),
]
# Every prefix through the new layout, including present option values, must
# report truncation rather than silently supplying defaults for missing data.
for end in range(19, len(snapshot)):
    malformed.append((f"status_snapshot_truncated_at_{end}", frame(snapshot[:end]), 1))

with (output / "protocol_v5.txt").open("w") as stream:
    for name, body in valid:
        stream.write(f"{name} {frame(body)}\n")
with (output / "protocol_v5_malformed.txt").open("w") as stream:
    for name, body, code in malformed:
        stream.write(f"{name} {body} {code}\n")
