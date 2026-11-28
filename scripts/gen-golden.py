#!/usr/bin/env python3
"""Generate the protocol fixtures independently of the Rust codec.

Writes protocol_v<VERSION>.txt (valid frames, one `name hex` per line) and
protocol_v<VERSION>_malformed.txt (`name hex error-code`) for the current
wire layout. The layout is spelled out here byte by byte, from
docs/developer/protocol.md, and never imported from the implementation.

When the layout changes: bump VERSION, change the builders below, keep the
cases that still test something, add cases for the new fields (including a
frame in the previous layout, which must now be refused), regenerate, point
crates/gpudog-core/tests/protocol_golden.rs at the new files and delete the
previous version's (steps in docs/developer/building.md). The changes of
each version are summarised in docs/developer/protocol.md; full earlier
layouts are in git history.

Usage: python3 scripts/gen-golden.py <existing-output-directory>
       python3 scripts/gen-golden.py --check
The canonical fixtures are in crates/gpudog-core/tests/golden/. --check,
run by `scripts/check.sh text` and CI, fails when they differ from what
this script writes, when another version's fixtures remain, or when
PROTOCOL_VERSION is not VERSION.
"""
from pathlib import Path
import re
import struct
import sys

VERSION = 8
# MAX_HOLD_LIMIT_MS: the bound on hold_limit_ms and idle_limit_ms.
MAX_HOLD = 0xFFFFFFFF


# Primitive encodings, all little-endian.
def u8(v): return struct.pack("<B", v)
def u16(v): return struct.pack("<H", v)
def u32(v): return struct.pack("<I", v & 0xFFFFFFFF)
def u64(v): return struct.pack("<Q", v)
def s(t): b = t.encode(); return u32(len(b)) + b
def gl(g): return u32(len(g)) + b"".join(u32(i) for i in g)
def ul(us): return u32(len(us)) + b"".join(s(x) for x in us)
def frame(body): return (u32(len(body)) + body).hex()


def option(value, encode):
    return u8(value is not None) + (encode(value) if value is not None else b"")


U0 = "GPU-8f6a1c2d-0000-1111-2222-333344445555"
U1 = "GPU-1C2D3E4F-aaaa-bbbb-cccc-ddddeeeeffff"
U2 = "GPU-static-3"


# Messages and their parts.
def acquire(sel, num, gpus, uuids, timeout, cmd, cands=()):
    return (u16(3) + u8(sel) + u32(num) + gl(gpus) + ul(uuids) + ul(list(cands))
            + u64(timeout) + s(cmd))


def granted(gpus, uuids, hold, idle=0):
    return u16(5) + gl(gpus) + ul(uuids) + u64(hold) + u64(idle)


def idle_stop(idle_ms):
    return u16(11) + u64(idle_ms)


def reservation(i, uid, pid, gpus, num, age, hold, idle_limit, idle, pos, cmd):
    return (u64(i) + u32(uid) + u32(pid) + gl(gpus) + u32(num) + u64(age)
            + u64(hold) + u64(idle_limit) + u64(idle) + u32(pos) + s(cmd))


def process(gpu=0, pid=0, uid=None, start=None, memory=None, ownership=3, command=""):
    return (u32(gpu) + u32(pid) + option(uid, u32) + option(start, u64)
            + option(memory, u64) + u8(ownership) + s(command))


def observation(observing=False, watch_ms=0, paused=False, age=None, error="",
                busy=(), processes=()):
    return (u8(observing) + u64(watch_ms) + u8(paused) + option(age, u64)
            + s(error) + gl(busy) + u32(len(processes)) + b"".join(processes))


def status(device_count=0, **kwargs):
    """A StatusReply with no device or reservation entries, not truncated."""
    return u16(10) + u32(device_count) + u32(0) * 3 + u8(0) + observation(**kwargs)


def status_full(holder_hold=3600000, holder_idle_limit=1800000, holder_idle=60000,
                waiting_hold=0, waiting_idle_limit=0):
    """A StatusReply with one device, one holder and one waiting request."""
    return (u16(10) + u32(4)
            + u32(2) + u32(0) + s("Test GPU") + s("GPU-abcdef") + u32(3) + s("") + s("")
            + u32(1) + reservation(9, 1000, -1, [0, 1], 2, 123456, holder_hold,
                                   holder_idle_limit, holder_idle, 0, "python train.py")
            + u32(1) + reservation(10, 1001, 7, [], 3, 0, waiting_hold,
                                   waiting_idle_limit, 0, 2, "")
            + u8(1) + observation())


snapshot = status(
    4, observing=True, watch_ms=1000, age=42, busy=[1, 2], processes=[
        process(0, 101, 1000, 123456, 4096, 0, "python train.py"),
        process(1, 102, 1000, 123457, 0, 1, "wrong GPU"),
        process(2, 103, 0, 0, None, 2, "unmanaged"),
        process(3, 104),
    ],
)

valid = [
    ("hello", u16(1) + u32(VERSION)),
    ("hello_ack", u16(2) + u32(VERSION)),
    ("acquire_count", acquire(0, 4, [], [], 30000, "python train.py --epochs 10",
                              [U0, U1, U2, "GPU-static-7"])),
    ("acquire_uuids", acquire(2, 2, [], [U0, U1], 0, "train")),
    ("acquire_candidates", acquire(0, 1, [], [], 0, "", [U1, U2])),
    ("queued", u16(4) + u32(7)),
    ("granted_unlimited", granted([2], [U2], 0)),
    ("granted", granted([0, 3, 1023], [U0, U1, U2], 86400000, 3600000)),
    ("idle_stop", idle_stop(1800000)),
    ("denied", u16(6) + u16(2) + s("no device freed")),
    ("ping", u16(7)),
    ("pong", u16(8)),
    ("status", u16(9)),
    ("status_reply_empty", status()),
    ("status_reply_full", status_full()),
    ("denied_unknown_reason", u16(6) + u16(0xEE) + s("future")),
    ("status_reply_healthy", status(observing=True, watch_ms=1, age=0)),
    ("status_reply_failed", status(observing=True, watch_ms=3600000,
                                   paused=True, error="NVML unavailable")),
    ("status_reply_process_snapshot", snapshot),
]

# Error codes: 1 truncated, 2 frame too large, 3 unknown type, 4 trailing
# bytes, 5 over a bound, 6 invalid value.
malformed = [
    # Framing.
    ("short_prefix", "0100", 1),
    ("oversized_prefix", "ffffffff", 2),
    ("empty_body", "00000000", 1),
    ("body_shorter_than_prefix", "030000000800", 1),
    ("unknown_type", frame(u16(0xEE)), 3),
    ("trailing_byte", frame(u16(7) + u8(0x99)), 4),
    # Lists and strings, through Granted and Denied.
    ("gpu_count_over_bound", frame(u16(5) + u32(0x401)), 5),
    ("gpu_count_unbacked", frame(u16(5) + u32(0x400)), 1),
    ("gpu_index_1024", frame(u16(5) + u32(1) + u32(1024)), 5),
    ("granted_empty", frame(granted([], [], 0)), 5),
    ("granted_duplicate", frame(granted([7, 7], [U0, U1], 0)), 6),
    ("granted_uuid_count_mismatch", frame(granted([0], [], 0)), 6),
    ("granted_uuid_malformed", frame(granted([0], ["not-a-uuid"], 0)), 6),
    ("granted_uuid_too_long", frame(granted([0], ["GPU-" + "a" * 93], 0)), 6),
    ("granted_uuid_duplicate", frame(granted([0, 1], [U0, U0.upper()], 0)), 6),
    ("uuid_count_over_bound", frame(u16(5) + gl([0]) + u32(0x401)), 5),
    ("uuid_count_unbacked", frame(u16(5) + gl([0]) + u32(2)), 1),
    ("string_over_bound", frame(u16(6) + u16(1) + u32(0x1001)), 5),
    ("string_unbacked", frame(u16(6) + u16(1) + u32(10) + b"abc"), 1),
    # Acquire.
    ("acquire_bad_selection", frame(acquire(0x7F, 1, [], [], 0, "")), 6),
    ("acquire_num_gpus_max", frame(acquire(0, 0xFFFFFFFF, [], [], 0, "")), 5),
    ("acquire_timeout_over", frame(acquire(0, 1, [], [], 0xFFFFFFFF + 1, "")), 5),
    ("acquire_bad_selection_and_truncated", frame(u16(3) + u8(0x7F) + u32(1)), 1),
    ("acquire_uuids_with_indices", frame(acquire(2, 1, [0], [U0], 0, "")), 6),
    ("acquire_uuids_count_mismatch", frame(acquire(2, 2, [], [U0], 0, "")), 6),
    ("acquire_count_with_uuids", frame(acquire(0, 1, [], [U0], 0, "")), 6),
    ("acquire_explicit_with_uuids", frame(acquire(1, 1, [0], [U0], 0, "")), 6),
    ("acquire_explicit_with_candidates", frame(acquire(1, 1, [0], [], 0, "", [U0])), 6),
    ("acquire_candidates_duplicate", frame(acquire(0, 1, [], [], 0, "", [U0, U0.lower()])), 6),
    ("acquire_candidates_malformed", frame(acquire(0, 1, [], [], 0, "", ["MIG-abc"])), 6),
    # StatusReply counts.
    ("status_reply_count_absurd", frame(u16(10) + u32(0) + u32(0xFFFFFFFF)), 5),
    ("status_reply_mid_entry", frame(u16(10) + u32(0) + u32(1) + u32(0)), 1),
    # Acquire by daemon GPU number is refused; count requests need candidates.
    ("acquire_legacy_indices", frame(acquire(1, 1, [0], [], 0, "")), 6),
    ("acquire_empty_candidates", frame(acquire(0, 1, [], [], 0, "")), 6),
    # A v4 StatusReply, which ends before the observation.
    ("status_reply_empty_legacy_v4", frame(u16(10) + u32(0) * 4 + u8(0)), 1),
]

# The observation: every flag and option tag takes only 0 or 1.
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
# Every prefix through the observation, including present option values,
# must report truncation rather than silently supplying defaults.
for end in range(19, len(snapshot)):
    malformed.append((f"status_snapshot_truncated_at_{end}", frame(snapshot[:end]), 1))

v6_granted = u16(5) + gl([0]) + ul([U0])
v7_granted = v6_granted + u64(0)
malformed += [
    # A v5 acquire still carries a priority before its command: the
    # priority's zero bytes are read as the command length, so 4 bytes trail.
    ("acquire_with_v5_priority",
     frame(acquire(0, 1, [], [], 0, "", [U0])[:-4] + u32(0) + s("")), 4),
    # Hold limits.
    ("granted_hold_limit_over", frame(granted([0], [U0], MAX_HOLD + 1)), 5),
    ("granted_hold_limit_u64_max", frame(granted([0], [U0], 2**64 - 1)), 5),
    # A v6 grant ends after its UUIDs: the limits are missing.
    ("granted_without_v7_hold_limit", frame(v6_granted), 1),
    ("granted_hold_limit_short", frame(v6_granted + u32(0)), 1),
    ("status_holder_hold_limit_over", frame(status_full(holder_hold=MAX_HOLD + 1)), 5),
    ("status_waiting_hold_limit_over", frame(status_full(waiting_hold=MAX_HOLD + 1)), 5),
    # Idle limits and IdleStop.
    ("granted_idle_limit_over", frame(granted([0], [U0], 0, MAX_HOLD + 1)), 5),
    ("granted_idle_limit_u64_max", frame(granted([0], [U0], 0, 2**64 - 1)), 5),
    # A v7 grant ends after its hold limit: the idle limit is missing.
    ("granted_without_v8_idle_limit", frame(v7_granted), 1),
    ("granted_idle_limit_short", frame(v7_granted + u32(0)), 1),
    ("status_holder_idle_limit_over",
     frame(status_full(holder_idle_limit=MAX_HOLD + 1)), 5),
    ("status_waiting_idle_limit_over",
     frame(status_full(waiting_idle_limit=MAX_HOLD + 1)), 5),
    ("idle_stop_empty", frame(u16(11)), 1),
    # status_reply_full cut inside the holder's idle fields, body bytes 104
    # to 119: type, device count and table (52 bytes), the holder count (4),
    # then the holder's id, uid, pid, gpu list, num_gpus, age and hold limit
    # (48).
    *((f"status_full_truncated_at_{cut}", frame(status_full()[:cut]), 1)
      for cut in (104, 106, 110, 112, 114, 118, 119)),
    ("idle_stop_short", frame(u16(11) + u32(0)), 1),
    ("idle_stop_trailing_byte", frame(idle_stop(0) + u8(0)), 4),
]

ROOT = Path(__file__).resolve().parent.parent
GOLDEN = ROOT / "crates/gpudog-core/tests/golden"
PROTOCOL_SOURCE = ROOT / "crates/gpudog-core/src/protocol/mod.rs"

files = {
    f"protocol_v{VERSION}.txt":
        "".join(f"{name} {frame(body)}\n" for name, body in valid),
    f"protocol_v{VERSION}_malformed.txt":
        "".join(f"{name} {body} {code}\n" for name, body, code in malformed),
}


def check():
    """Problems with the committed fixtures, one line each."""
    problems = []
    for name, text in files.items():
        path = GOLDEN / name
        if not path.is_file():
            problems.append(f"{path.relative_to(ROOT)}: missing")
        elif path.read_bytes() != text.encode():
            problems.append(f"{path.relative_to(ROOT)}: differs from what "
                            "scripts/gen-golden.py writes")
    for path in sorted(GOLDEN.glob("protocol_v*.txt")):
        if path.name not in files:
            problems.append(f"{path.relative_to(ROOT)}: not version {VERSION}; delete it")
    # The same pattern as in scripts/check-repo-text.py.
    match = re.search(r"pub const PROTOCOL_VERSION: u32 = (\d+);",
                      PROTOCOL_SOURCE.read_text())
    if not match or int(match.group(1)) != VERSION:
        found = match.group(1) if match else "no PROTOCOL_VERSION"
        problems.append(f"{PROTOCOL_SOURCE.relative_to(ROOT)}: {found}, but "
                        f"scripts/gen-golden.py has VERSION = {VERSION}")
    return problems


if sys.argv[1:] == ["--check"]:
    problems = check()
    for problem in problems:
        print(problem, file=sys.stderr)
    if problems:
        sys.exit("fix the above; regenerate the fixtures with "
                 "python3 scripts/gen-golden.py crates/gpudog-core/tests/golden")
    print(f"protocol fixtures match scripts/gen-golden.py (v{VERSION})")
elif len(sys.argv) == 2 and Path(sys.argv[1]).is_dir():
    for name, text in files.items():
        (Path(sys.argv[1]) / name).write_bytes(text.encode())
else:
    sys.exit(f"usage: {sys.argv[0]} <existing-output-directory> | --check")
