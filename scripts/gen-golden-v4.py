#!/usr/bin/env python3
"""Generate v4 fixtures independently, preserving the historical v3 bytes.

Usage: python3 scripts/gen-golden-v4.py <existing-output-directory>
"""
from pathlib import Path
import runpy
import sys
import tempfile

root = Path(__file__).resolve().parent.parent
output = Path(sys.argv[1])
(root / "tmp").mkdir(exist_ok=True)
with tempfile.TemporaryDirectory(prefix="golden-v3-", dir=root / "tmp") as scratch:
    sys.argv[1] = scratch
    v3 = runpy.run_path(str(root / "scripts/gen-golden-v3.py"))

u16, u32, acquire, frame = (v3[key] for key in ("u16", "u32", "acquire", "frame"))
u0, u1, u2 = (v3[key] for key in ("U0", "U1", "U2"))
valid = [(name, body) for name, body in v3["valid"]
         if name not in {"hello", "hello_ack", "acquire_count", "acquire_explicit"}]
valid = [
    ("hello", u16(1) + u32(4)),
    ("hello_ack", u16(2) + u32(4)),
    ("acquire_count", acquire(0, 4, [], [], 30000, -20,
                              "python train.py --epochs 10", [u0, u1, u2, "GPU-static-7"])),
] + valid
malformed = v3["malformed"] + [
    ("acquire_legacy_indices", frame(acquire(1, 1, [0], [], 0, 0, "")), 6),
    ("acquire_empty_candidates", frame(acquire(0, 1, [], [], 0, 0, "")), 6),
]
with (output / "protocol_v4.txt").open("w") as stream:
    for name, body in valid:
        stream.write(f"{name} {frame(body)}\n")
with (output / "protocol_v4_malformed.txt").open("w") as stream:
    for name, body, code in malformed:
        stream.write(f"{name} {body} {code}\n")
