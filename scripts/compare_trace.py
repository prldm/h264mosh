"""Cross-check h264mosh's slice header parsing against ffmpeg's trace_headers filter.

For every slice, compares where the header ends (bit position) and a few field values.
usage: python scripts/compare_trace.py file.264 [file2.264 ...]
"""
import os
import re
import subprocess
import sys

BIN = os.path.join("target", "release", "h264mosh" + (".exe" if os.name == "nt" else ""))
LINE = re.compile(r"^\[trace_headers @ [0-9a-fx]+\] (\d+)\s+(\S+)\s+([01]+) = (-?\d+)")
FIELDS = ["first_mb_in_slice", "slice_type", "frame_num", "pic_order_cnt_lsb", "slice_qp_delta", "disable_deblocking_filter_idc"]


def ffmpeg_slices(path):
    out = subprocess.run(
        ["ffmpeg", "-v", "trace", "-nostdin", "-i", path, "-c", "copy", "-bsf:v", "trace_headers", "-f", "null", "-"],
        capture_output=True, text=True, errors="replace",
    ).stderr
    slices, cur = [], None
    for line in out.splitlines():
        if line.endswith("] Slice Header"):
            cur = {"end": 0}
            slices.append(cur)
            continue
        m = LINE.match(line)
        if not m and "[trace_headers" in line:
            cur = None  # any other section (SEI, filler, parameter sets, packet info)
            continue
        if cur is None or not m:
            continue
        pos, name, bits, value = int(m[1]), m[2], m[3], int(m[4])
        if name == "cabac_alignment_one_bit" or name in ("forbidden_zero_bit", "nal_ref_idc"):
            continue
        cur["end"] = pos + len(bits)
        cur.setdefault(name, value)
    return slices


def our_slices(path):
    out = subprocess.run([BIN, "slices", path], capture_output=True, text=True, check=True).stdout.splitlines()
    head = out[0].split(",")
    return [dict(zip(head, map(int, l.split(",")))) for l in out[1:]]


def compare(path):
    theirs, ours = ffmpeg_slices(path), our_slices(path)
    if len(theirs) != len(ours):
        print(f"FAIL {path}: ffmpeg saw {len(theirs)} slices, we saw {len(ours)}")
        return False
    bad = 0
    for i, (t, o) in enumerate(zip(theirs, ours)):
        diffs = []
        if t["end"] != o["header_end_bit"]:
            diffs.append(f"header end {o['header_end_bit']} vs ffmpeg {t['end']}")
        for f in FIELDS:
            if f in t and t[f] != o[f]:
                diffs.append(f"{f} {o[f]} vs ffmpeg {t[f]}")
        if diffs:
            bad += 1
            if bad <= 5:
                print(f"  slice {i}: " + "; ".join(diffs))
    print(f"{'ok  ' if bad == 0 else 'FAIL'} {path}: {len(ours)} slices, {bad} mismatching")
    return bad == 0


if __name__ == "__main__":
    results = [compare(p) for p in sys.argv[1:]]
    sys.exit(0 if all(results) else 1)
