#!/usr/bin/env python3
"""Generate v7 fixtures independently, retaining the historical v3-v6 bytes.

v7 appends a u64 hold_limit_ms to Granted, and inserts one after age_ms in
each ReservationInfo of StatusReply. Every frame that carries either is
rebuilt here; the others are reused from v6 unchanged.

Usage: python3 scripts/gen-golden-v7.py <existing-output-directory>
"""
from pathlib import Path
import runpy
import sys
import tempfile

root = Path(__file__).resolve().parent.parent
output = Path(sys.argv[1])
(root / "tmp").mkdir(exist_ok=True)
with tempfile.TemporaryDirectory(prefix="golden-v6-", dir=root / "tmp") as scratch:
    sys.argv[1] = scratch
    v6 = runpy.run_path(str(root / "scripts/gen-golden-v6.py"))

u8, u16, u32, u64, s, gl, ul, frame = (
    v6[key] for key in ("u8", "u16", "u32", "u64", "s", "gl", "ul", "frame")
)
U0, U1, U2 = (v6[key] for key in ("U0", "U1", "U2"))
observation = v6["observation"]
MAX_HOLD = 0xFFFFFFFF


def granted(gpus, uuids, hold):
    return u16(5) + gl(gpus) + ul(uuids) + u64(hold)


def reservation(i, uid, pid, gpus, num, age, hold, pos, cmd):
    return (u64(i) + u32(uid) + u32(pid) + gl(gpus) + u32(num) + u64(age)
            + u64(hold) + u32(pos) + s(cmd))


def status_full(holder_hold=3600000, waiting_hold=0):
    return (u16(10) + u32(4)
            + u32(2) + u32(0) + s("Test GPU") + s("GPU-abcdef") + u32(3) + s("") + s("")
            + u32(1) + reservation(9, 1000, -1, [0, 1], 2, 123456, holder_hold, 0,
                                   "python train.py")
            + u32(1) + reservation(10, 1001, 7, [], 3, 0, waiting_hold, 2, "")
            + u8(1) + observation())


rebuilt = {
    "hello": u16(1) + u32(7),
    "hello_ack": u16(2) + u32(7),
    "granted": granted([0, 3, 1023], [U0, U1, U2], 86400000),
    "status_reply_full": status_full(),
}
valid = []
for name, body in v6["valid"]:
    if name == "granted":
        valid.append(("granted_unlimited", granted([2], [U2], 0)))
    valid.append((name, rebuilt.get(name, body)))
assert all(name in dict(valid) for name in rebuilt), "a rebuilt frame has no v6 entry"

# Malformed Granted frames would now fail as truncated before their own
# defect is reached; rebuild them with the same defect and a zero limit.
granted_cases = {
    "granted_empty": (granted([], [], 0), 5),
    "granted_duplicate": (granted([7, 7], [U0, U1], 0), 6),
    "granted_uuid_count_mismatch": (granted([0], [], 0), 6),
    "granted_uuid_malformed": (granted([0], ["not-a-uuid"], 0), 6),
    "granted_uuid_too_long": (granted([0], ["GPU-" + "a" * 93], 0), 6),
    "granted_uuid_duplicate": (granted([0, 1], [U0, U0.upper()], 0), 6),
}
malformed = []
for name, body, code in v6["malformed"]:
    if name in granted_cases:
        body, code = granted_cases.pop(name)
        body = frame(body)
    malformed.append((name, body, code))
assert not granted_cases, f"no v6 entry for {sorted(granted_cases)}"
v6_granted = u16(5) + gl([0]) + ul([U0])
malformed += [
    ("granted_hold_limit_over", frame(granted([0], [U0], MAX_HOLD + 1)), 5),
    ("granted_hold_limit_u64_max", frame(granted([0], [U0], 2**64 - 1)), 5),
    # A v6 grant ends after its UUIDs: read as v7, the limit is missing.
    ("granted_without_v7_hold_limit", frame(v6_granted), 1),
    ("granted_hold_limit_short", frame(v6_granted + u32(0)), 1),
    ("status_holder_hold_limit_over", frame(status_full(holder_hold=MAX_HOLD + 1)), 5),
    ("status_waiting_hold_limit_over", frame(status_full(waiting_hold=MAX_HOLD + 1)), 5),
]

with (output / "protocol_v7.txt").open("w") as stream:
    for name, body in valid:
        stream.write(f"{name} {frame(body)}\n")
with (output / "protocol_v7_malformed.txt").open("w") as stream:
    for name, body, code in malformed:
        stream.write(f"{name} {body} {code}\n")
