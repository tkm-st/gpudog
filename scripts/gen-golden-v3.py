#!/usr/bin/env python3
"""Generate protocol-v3 fixtures independently of the implementation.

Usage: python3 scripts/gen-golden-v3.py <existing-output-directory>
The canonical fixtures are in crates/gpudog-lib/tests/golden/.
"""
import struct
import sys

def u8(v): return struct.pack('<B', v)
def u16(v): return struct.pack('<H', v)
def u32(v): return struct.pack('<I', v & 0xFFFFFFFF)
def u64(v): return struct.pack('<Q', v)
def s(t): b = t.encode(); return u32(len(b)) + b
def gl(g): return u32(len(g)) + b''.join(u32(i) for i in g)
def ul(us): return u32(len(us)) + b''.join(s(x) for x in us)
def frame(body): return (u32(len(body)) + body).hex()

U0 = 'GPU-8f6a1c2d-0000-1111-2222-333344445555'
U1 = 'GPU-1C2D3E4F-aaaa-bbbb-cccc-ddddeeeeffff'
U2 = 'GPU-static-3'

def acquire(sel, num, gpus, uuids, timeout, prio, cmd, cands=()):
    return u16(3) + u8(sel) + u32(num) + gl(gpus) + ul(uuids) + ul(list(cands)) + u64(timeout) + u32(prio) + s(cmd)

def reservation(i, uid, pid, prio, gpus, num, age, pos, cmd):
    return u64(i) + u32(uid) + u32(pid) + u32(prio) + gl(gpus) + u32(num) + u64(age) + u32(pos) + s(cmd)

valid = [
    ('hello', u16(1) + u32(3)),
    ('hello_ack', u16(2) + u32(3)),
    ('acquire_count', acquire(0, 4, [], [], 30000, -20, 'python train.py --epochs 10')),
    ('acquire_explicit', acquire(1, 2, [2, 5], [], 0xFFFFFFFF, 0, '')),
    ('acquire_uuids', acquire(2, 2, [], [U0, U1], 0, 3, 'train')),
    ('acquire_candidates', acquire(0, 1, [], [], 0, 0, '', [U1, U2])),
    ('queued', u16(4) + u32(7)),
    ('granted', u16(5) + gl([0, 3, 1023]) + ul([U0, U1, U2])),
    ('denied', u16(6) + u16(2) + s('no device freed')),
    ('ping', u16(7)),
    ('pong', u16(8)),
    ('status', u16(9)),
    ('status_reply_empty', u16(10) + u32(0) + u32(0) + u32(0) + u32(0) + u8(0)),
    ('status_reply_full', u16(10) + u32(4)
        + u32(2) + u32(0) + s('Test GPU') + s('GPU-abcdef') + u32(3) + s('') + s('')
        + u32(1) + reservation(9, 1000, -1, -20, [0, 1], 2, 123456, 0, 'python train.py')
        + u32(1) + reservation(10, 1001, 7, 19, [], 3, 0, 2, '')
        + u8(1)),
    ('denied_unknown_reason', u16(6) + u16(0xEE) + s('future')),
]

def raw(hexbody): return hexbody

malformed = [
    ('short_prefix', '0100', 1),
    ('oversized_prefix', 'ffffffff', 2),
    ('empty_body', '00000000', 1),
    ('body_shorter_than_prefix', '030000000800', 1),
    ('unknown_type', frame(u16(0xEE)), 3),
    ('trailing_byte', frame(u16(7) + u8(0x99)), 4),
    ('gpu_count_over_bound', frame(u16(5) + u32(0x401)), 5),
    ('gpu_count_unbacked', frame(u16(5) + u32(0x400)), 1),
    ('gpu_index_1024', frame(u16(5) + u32(1) + u32(1024)), 5),
    ('granted_empty', frame(u16(5) + gl([]) + ul([])), 5),
    ('granted_duplicate', frame(u16(5) + gl([7, 7]) + ul([U0, U1])), 6),
    ('granted_uuid_count_mismatch', frame(u16(5) + gl([0]) + ul([])), 6),
    ('granted_uuid_malformed', frame(u16(5) + gl([0]) + ul(['not-a-uuid'])), 6),
    ('granted_uuid_too_long', frame(u16(5) + gl([0]) + ul(['GPU-' + 'a' * 93])), 6),
    ('granted_uuid_duplicate', frame(u16(5) + gl([0, 1]) + ul([U0, U0.upper()])), 6),
    ('uuid_count_over_bound', frame(u16(5) + gl([0]) + u32(0x401)), 5),
    ('uuid_count_unbacked', frame(u16(5) + gl([0]) + u32(2)), 1),
    ('string_over_bound', frame(u16(6) + u16(1) + u32(0x1001)), 5),
    ('string_unbacked', frame(u16(6) + u16(1) + u32(10) + b'abc'), 1),
    ('acquire_bad_selection', frame(acquire(0x7F, 1, [], [], 0, 0, '')), 6),
    ('acquire_num_gpus_max', frame(acquire(0, 0xFFFFFFFF, [], [], 0, 0, '')), 5),
    ('acquire_timeout_over', frame(acquire(0, 1, [], [], 0xFFFFFFFF + 1, 0, '')), 5),
    ('acquire_bad_selection_and_truncated', frame(u16(3) + u8(0x7F) + u32(1)), 1),
    ('acquire_uuids_with_indices', frame(acquire(2, 1, [0], [U0], 0, 0, '')), 6),
    ('acquire_uuids_count_mismatch', frame(acquire(2, 2, [], [U0], 0, 0, '')), 6),
    ('acquire_count_with_uuids', frame(acquire(0, 1, [], [U0], 0, 0, '')), 6),
    ('acquire_explicit_with_uuids', frame(acquire(1, 1, [0], [U0], 0, 0, '')), 6),
    ('acquire_explicit_with_candidates', frame(acquire(1, 1, [0], [], 0, 0, '', [U0])), 6),
    ('acquire_candidates_duplicate', frame(acquire(0, 1, [], [], 0, 0, '', [U0, U0.lower()])), 6),
    ('acquire_candidates_malformed', frame(acquire(0, 1, [], [], 0, 0, '', ['MIG-abc'])), 6),
    ('status_reply_count_absurd', frame(u16(10) + u32(0) + u32(0xFFFFFFFF)), 5),
    ('status_reply_mid_entry', frame(u16(10) + u32(0) + u32(1) + u32(0)), 1),
]

out = sys.argv[1]
with open(f'{out}/protocol_v3.txt', 'w') as f:
    for name, body in valid:
        f.write(f'{name} {frame(body)}\n')
with open(f'{out}/protocol_v3_malformed.txt', 'w') as f:
    for name, hexframe, code in malformed:
        f.write(f'{name} {hexframe} {code}\n')
