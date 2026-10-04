"""Throwaway feasibility prototype: H.264 Annex B frame-level datamosh.

usage: proto.py in.h264 out.h264 drop_idr  <frame_idx>
       proto.py in.h264 out.h264 dup       <frame_idx> <count>
"""
import sys


def split_nals(data):
    out, i, n = [], 0, len(data)
    starts = []
    while True:
        j = data.find(b"\x00\x00\x01", i)
        if j < 0:
            break
        starts.append(j + 3)
        i = j + 3
    for k, s in enumerate(starts):
        e = starts[k + 1] - 3 if k + 1 < len(starts) else n
        nal = data[s:e]
        while nal.endswith(b"\x00"):  # trailing zero / 4-byte start code
            nal = nal[:-1]
        out.append(nal)
    return out


def ebsp_to_rbsp(b):
    out, zeros = bytearray(), 0
    for x in b:
        if zeros >= 2 and x == 3:
            zeros = 0
            continue
        out.append(x)
        zeros = zeros + 1 if x == 0 else 0
    return out


def rbsp_to_ebsp(b):
    out, zeros = bytearray(), 0
    for x in b:
        if zeros >= 2 and x <= 3:
            out.append(3)
            zeros = 0
        out.append(x)
        zeros = zeros + 1 if x == 0 else 0
    return out


class BR:
    def __init__(self, b):
        self.b, self.pos = b, 0

    def u(self, n):
        v = 0
        for _ in range(n):
            v = (v << 1) | ((self.b[self.pos >> 3] >> (7 - (self.pos & 7))) & 1)
            self.pos += 1
        return v

    def ue(self):
        z = 0
        while self.u(1) == 0:
            z += 1
        return (1 << z) - 1 + self.u(z)

    def se(self):
        k = self.ue()
        return (k + 1) // 2 if k & 1 else -(k // 2)


def write_bits(buf, pos, n, val):
    for i in range(n):
        bit = (val >> (n - 1 - i)) & 1
        byte, sh = (pos + i) >> 3, 7 - ((pos + i) & 7)
        buf[byte] = (buf[byte] & ~(1 << sh)) | (bit << sh)


def parse_sps(rbsp):
    r = BR(rbsp)
    r.pos = 8  # nal header
    profile = r.u(8); r.u(8); r.u(8); r.ue()
    sep_colour = 0
    if profile in (100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135):
        cf = r.ue()
        if cf == 3:
            sep_colour = r.u(1)
        r.ue(); r.ue(); r.u(1)
        if r.u(1):
            raise NotImplementedError("scaling matrix in SPS")
    log2_fn = r.ue() + 4
    poc_type = r.ue()
    log2_poc = r.ue() + 4 if poc_type == 0 else None
    if poc_type == 1:
        raise NotImplementedError("poc type 1")
    return dict(log2_fn=log2_fn, poc_type=poc_type, log2_poc=log2_poc, sep_colour=sep_colour)


def slice_fields(rbsp, sps, nal_type):
    """Return bit positions of frame_num and pic_order_cnt_lsb."""
    r = BR(rbsp)
    r.pos = 8
    first_mb = r.ue(); slice_type = r.ue(); r.ue()
    if sps["sep_colour"]:
        r.u(2)
    fn_pos = r.pos
    r.u(sps["log2_fn"])
    # assumes frame_mbs_only (progressive) -> no field_pic_flag
    if nal_type == 5:
        r.ue()
    poc_pos = r.pos if sps["poc_type"] == 0 else None
    return first_mb, slice_type % 5, fn_pos, poc_pos


def main():
    inp, outp, mode = sys.argv[1:4]
    nals = split_nals(open(inp, "rb").read())
    sps = None
    # group into access units: [list of nal bytes], each AU = one coded picture
    headers, frames, cur = [], [], None
    for nal in nals:
        t = nal[0] & 0x1F
        if t == 7:
            sps = parse_sps(ebsp_to_rbsp(nal))
        if t in (1, 5):
            first_mb = BR(ebsp_to_rbsp(nal[:8])); first_mb.pos = 8
            if first_mb.ue() == 0:
                cur = {"idr": t == 5, "nals": []}
                frames.append(cur)
            cur["nals"].append(nal)
        elif t in (7, 8) and not frames:
            headers.append(nal)
        # drop SEI/AUD/repeated SPS/PPS for simplicity

    print("sps:", sps, "frames:", len(frames), "IDR at", [i for i, f in enumerate(frames) if f["idr"]])

    if mode == "drop_idr":
        k = int(sys.argv[4])
        assert frames[k]["idr"], "frame %d is not IDR" % k
        del frames[k]
    elif mode == "dup":
        k, cnt = int(sys.argv[4]), int(sys.argv[5])
        assert not frames[k]["idr"]
        frames[k + 1:k + 1] = [frames[k]] * cnt

    # renumber frame_num / poc so the decoder sees one continuous GOP after the first IDR
    out = bytearray()
    for h in headers:
        out += b"\x00\x00\x00\x01" + h
    idx = 0
    for f in frames:
        if f["idr"]:
            idx = 0
        for nal in f["nals"]:
            t = nal[0] & 0x1F
            rb = ebsp_to_rbsp(nal)
            _, _, fn_pos, poc_pos = slice_fields(rb, sps, t)
            if t == 1:  # IDR keeps frame_num 0
                write_bits(rb, fn_pos, sps["log2_fn"], idx % (1 << sps["log2_fn"]))
            if poc_pos is not None:
                write_bits(rb, poc_pos, sps["log2_poc"], (2 * idx) % (1 << sps["log2_poc"]))
            out += b"\x00\x00\x00\x01" + rbsp_to_ebsp(rb)
        idx += 1
    open(outp, "wb").write(out)


main()
