#!/usr/bin/env python3
"""Generate v6 fixtures independently, retaining the historical v3-v5 bytes.

v6 removes the client-chosen priority (an i32) from Acquire and from each
ReservationInfo in StatusReply. Every frame that carried one is rebuilt
here without it; the others are reused from v5 unchanged.

Usage: python3 scripts/gen-golden-v6.py <existing-output-directory>
"""
from pathlib import Path
import runpy
import sys
import tempfile

root = Path(__file__).resolve().parent.parent
output = Path(sys.argv[1])
(root / "tmp").mkdir(exist_ok=True)
with tempfile.TemporaryDirectory(prefix="golden-v5-", dir=root / "tmp") as scratch:
    sys.argv[1] = scratch
    v5 = runpy.run_path(str(root / "scripts/gen-golden-v5.py"))
    v3 = v5["v3"]

u8, u16, u32, u64, s, gl, ul, frame = (
    v3[key] for key in ("u8", "u16", "u32", "u64", "s", "gl", "ul", "frame")
)
U0, U1, U2 = (v3[key] for key in ("U0", "U1", "U2"))
observation = v5["observation"]


def acquire(sel, num, gpus, uuids, timeout, cmd, cands=()):
    return (u16(3) + u8(sel) + u32(num) + gl(gpus) + ul(uuids) + ul(list(cands))
            + u64(timeout) + s(cmd))


def reservation(i, uid, pid, gpus, num, age, pos, cmd):
    return (u64(i) + u32(uid) + u32(pid) + gl(gpus) + u32(num) + u64(age)
            + u32(pos) + s(cmd))


rebuilt = {
    "hello": u16(1) + u32(6),
    "hello_ack": u16(2) + u32(6),
    "acquire_count": acquire(0, 4, [], [], 30000, "python train.py --epochs 10",
                             [U0, U1, U2, "GPU-static-7"]),
    "acquire_uuids": acquire(2, 2, [], [U0, U1], 0, "train"),
    "acquire_candidates": acquire(0, 1, [], [], 0, "", [U1, U2]),
    "status_reply_full": (
        u16(10) + u32(4)
        + u32(2) + u32(0) + s("Test GPU") + s("GPU-abcdef") + u32(3) + s("") + s("")
        + u32(1) + reservation(9, 1000, -1, [0, 1], 2, 123456, 0, "python train.py")
        + u32(1) + reservation(10, 1001, 7, [], 3, 0, 2, "")
        + u8(1) + observation()),
}
valid = [(name, rebuilt.get(name, body)) for name, body in v5["valid"]]
assert all(name in dict(valid) for name in rebuilt), "a rebuilt frame has no v5 entry"

# Malformed acquire frames carried a priority; rebuild them with the same
# defect. The raw selection prefix and every status case are unaffected,
# except the legacy v4 full status body, whose reservations carry one too.
acquire_cases = {
    "acquire_bad_selection": (acquire(0x7F, 1, [], [], 0, ""), 6),
    "acquire_num_gpus_max": (acquire(0, 0xFFFFFFFF, [], [], 0, ""), 5),
    "acquire_timeout_over": (acquire(0, 1, [], [], 0xFFFFFFFF + 1, ""), 5),
    "acquire_uuids_with_indices": (acquire(2, 1, [0], [U0], 0, ""), 6),
    "acquire_uuids_count_mismatch": (acquire(2, 2, [], [U0], 0, ""), 6),
    "acquire_count_with_uuids": (acquire(0, 1, [], [U0], 0, ""), 6),
    "acquire_explicit_with_uuids": (acquire(1, 1, [0], [U0], 0, ""), 6),
    "acquire_explicit_with_candidates": (acquire(1, 1, [0], [], 0, "", [U0]), 6),
    "acquire_candidates_duplicate": (acquire(0, 1, [], [], 0, "", [U0, U0.lower()]), 6),
    "acquire_candidates_malformed": (acquire(0, 1, [], [], 0, "", ["MIG-abc"]), 6),
    "acquire_legacy_indices": (acquire(1, 1, [0], [], 0, ""), 6),
    "acquire_empty_candidates": (acquire(0, 1, [], [], 0, ""), 6),
}
malformed = []
for name, body, code in v5["malformed"]:
    if name in acquire_cases:
        body, code = acquire_cases.pop(name)
        body = frame(body)
    elif name == "status_reply_full_legacy_v4":
        continue
    malformed.append((name, body, code))
assert not acquire_cases, f"no v5 entry for {sorted(acquire_cases)}"
malformed += [
    # A v5 acquire still carries a priority before its command: read as v6,
    # the priority's zero bytes are the command length, so 4 bytes trail.
    ("acquire_with_v5_priority",
     frame(acquire(0, 1, [], [], 0, "", [U0])[:-4] + u32(0) + s("")), 4),
]

with (output / "protocol_v6.txt").open("w") as stream:
    for name, body in valid:
        stream.write(f"{name} {frame(body)}\n")
with (output / "protocol_v6_malformed.txt").open("w") as stream:
    for name, body, code in malformed:
        stream.write(f"{name} {body} {code}\n")
