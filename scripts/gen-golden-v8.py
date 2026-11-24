#!/usr/bin/env python3
"""Generate v8 fixtures independently, retaining the historical v3-v7 bytes.

v8 appends a u64 idle_limit_ms to Granted, inserts u64 idle_limit_ms and
u64 idle_ms after hold_limit_ms in each ReservationInfo of StatusReply, and
adds the IdleStop message (type 11, one u64 idle_ms). Every frame that
carries Granted or a ReservationInfo is rebuilt here; the others are reused
from v7 unchanged.

Usage: python3 scripts/gen-golden-v8.py <existing-output-directory>
"""
from pathlib import Path
import runpy
import sys
import tempfile

root = Path(__file__).resolve().parent.parent
output = Path(sys.argv[1])
(root / "tmp").mkdir(exist_ok=True)
with tempfile.TemporaryDirectory(prefix="golden-v7-", dir=root / "tmp") as scratch:
    sys.argv[1] = scratch
    v7 = runpy.run_path(str(root / "scripts/gen-golden-v7.py"))

u8, u16, u32, u64, s, gl, ul, frame = (
    v7[key] for key in ("u8", "u16", "u32", "u64", "s", "gl", "ul", "frame")
)
U0, U1, U2 = (v7[key] for key in ("U0", "U1", "U2"))
observation = v7["observation"]
MAX_HOLD = v7["MAX_HOLD"]


def granted(gpus, uuids, hold, idle=0):
    return u16(5) + gl(gpus) + ul(uuids) + u64(hold) + u64(idle)


def reservation(i, uid, pid, gpus, num, age, hold, idle_limit, idle, pos, cmd):
    return (u64(i) + u32(uid) + u32(pid) + gl(gpus) + u32(num) + u64(age)
            + u64(hold) + u64(idle_limit) + u64(idle) + u32(pos) + s(cmd))


def status_full(holder_hold=3600000, holder_idle_limit=1800000, holder_idle=60000,
                waiting_hold=0, waiting_idle_limit=0):
    return (u16(10) + u32(4)
            + u32(2) + u32(0) + s("Test GPU") + s("GPU-abcdef") + u32(3) + s("") + s("")
            + u32(1) + reservation(9, 1000, -1, [0, 1], 2, 123456, holder_hold,
                                   holder_idle_limit, holder_idle, 0, "python train.py")
            + u32(1) + reservation(10, 1001, 7, [], 3, 0, waiting_hold,
                                   waiting_idle_limit, 0, 2, "")
            + u8(1) + observation())


def idle_stop(idle_ms):
    return u16(11) + u64(idle_ms)


rebuilt = {
    "hello": u16(1) + u32(8),
    "hello_ack": u16(2) + u32(8),
    "granted_unlimited": granted([2], [U2], 0),
    "granted": granted([0, 3, 1023], [U0, U1, U2], 86400000, 3600000),
    "status_reply_full": status_full(),
}
valid = []
for name, body in v7["valid"]:
    valid.append((name, rebuilt.get(name, body)))
    if name == "granted":
        valid.append(("idle_stop", idle_stop(1800000)))
assert all(name in dict(valid) for name in rebuilt), "a rebuilt frame has no v7 entry"

# Malformed Granted frames would now fail as truncated before their own
# defect is reached; rebuild them with the same defect and zero limits.
granted_cases = {
    "granted_empty": (granted([], [], 0), 5),
    "granted_duplicate": (granted([7, 7], [U0, U1], 0), 6),
    "granted_uuid_count_mismatch": (granted([0], [], 0), 6),
    "granted_uuid_malformed": (granted([0], ["not-a-uuid"], 0), 6),
    "granted_uuid_too_long": (granted([0], ["GPU-" + "a" * 93], 0), 6),
    "granted_uuid_duplicate": (granted([0, 1], [U0, U0.upper()], 0), 6),
    "granted_hold_limit_over": (granted([0], [U0], MAX_HOLD + 1), 5),
    "granted_hold_limit_u64_max": (granted([0], [U0], 2**64 - 1), 5),
    "status_holder_hold_limit_over": (status_full(holder_hold=MAX_HOLD + 1), 5),
    "status_waiting_hold_limit_over": (status_full(waiting_hold=MAX_HOLD + 1), 5),
}
malformed = []
for name, body, code in v7["malformed"]:
    if name in granted_cases:
        body, code = granted_cases.pop(name)
        body = frame(body)
    malformed.append((name, body, code))
assert not granted_cases, f"no v7 entry for {sorted(granted_cases)}"
v7_granted = u16(5) + gl([0]) + ul([U0]) + u64(0)
malformed += [
    ("granted_idle_limit_over", frame(granted([0], [U0], 0, MAX_HOLD + 1)), 5),
    ("granted_idle_limit_u64_max", frame(granted([0], [U0], 0, 2**64 - 1)), 5),
    # A v7 grant ends after its hold limit: read as v8, the idle limit is
    # missing.
    ("granted_without_v8_idle_limit", frame(v7_granted), 1),
    ("granted_idle_limit_short", frame(v7_granted + u32(0)), 1),
    ("status_holder_idle_limit_over",
     frame(status_full(holder_idle_limit=MAX_HOLD + 1)), 5),
    ("status_waiting_idle_limit_over",
     frame(status_full(waiting_idle_limit=MAX_HOLD + 1)), 5),
    ("idle_stop_empty", frame(u16(11)), 1),
    # status_reply_full cut inside the holder's v8 idle fields, body bytes
    # 104 to 119: type, device count and table (52 bytes), the holder count
    # (4), then the holder's id, uid, pid, gpu list, num_gpus, age and hold
    # limit (48).
    *((f"status_full_truncated_at_{cut}", frame(status_full()[:cut]), 1)
      for cut in (104, 106, 110, 112, 114, 118, 119)),
    ("idle_stop_short", frame(u16(11) + u32(0)), 1),
    ("idle_stop_trailing_byte", frame(idle_stop(0) + u8(0)), 4),
]

with (output / "protocol_v8.txt").open("w") as stream:
    for name, body in valid:
        stream.write(f"{name} {frame(body)}\n")
with (output / "protocol_v8_malformed.txt").open("w") as stream:
    for name, body, code in malformed:
        stream.write(f"{name} {body} {code}\n")
