"""Generate src/cabac/tables.rs: the CABAC context initialisation values (m, n) of
ITU-T H.264 clause 9.3.1.1, Tables 9-12 to 9-33.

The source is the standard itself (tested with the V16, 06/2026 edition PDF).  Two external
tools are used as subprocesses:

  * `pdftotext -layout` (once, over the whole PDF) only to find which pages carry the
    titles "Table 9-NN - Values of variables m and n for ctxIdx from ...";
  * `mutool draw -F stext` (per table page) to get every character with its coordinates.

The numbers of the PDF cannot be taken from plain text: in the narrow cells of Tables
9-12 to 9-17 (ctxIdx in columns, rows m / n per cabac_init_idc) a number such as -46 wraps
into "-4" and, one line below, "6".  Words are therefore assigned to cells by position:
each fragment goes to the header ctxIdx column whose centre is nearest in x and to the
m / n row whose y-band it falls in, and the fragments of a cell are concatenated top to
bottom.  Tables 9-18 to 9-33 are laid out the other way round (a row per ctxIdx with the
pairs m n for I slices and cabac_init_idc 0, 1, 2, often in two ctxIdx column groups side
by side, several of them continued over page breaks); there every text line of purely
integer tokens is a data row of 9 or 18 tokens.  Every table must yield exactly the
expected ctxIdx set, otherwise the script stops with an error.  One known misprint of the
PDF is corrected explicitly (TYPOS: ctxIdx "641" in Table 9-26 is 541).

Entries that a slice type does not use are (0, 0) in the generated tables: ctxIdx 11..=59
for I and SI slices (Tables 9-13 to 9-16 give no I values; Table 9-11), and ctxIdx 276
(end_of_slice_flag / I_PCM bin, decoded by the terminate process, no context variable) for
all.  Tables valid for all slice types (9-12, 9-17 and every row-layout table) go into
all four sets.

Independent check: `--check <FFmpeg libavcodec/h264_cabac.c>` parses FFmpeg's
cabac_context_init_I / cabac_context_init_PB, compares every entry the standard defines
for the slice type, lists differences in unused entries as information only, and prints
the FNV-1a fingerprint of the table.  FFmpeg's copy is used only for this comparison,
nothing of it is copied into the repository.  The fingerprint is pinned in PINNED_FNV1A
below after a successful --check; generation refuses to run if the table extracted from
the PDF does not hash to it, and the same constant is compiled into the Rust tests.

usage: python scripts/gen_cabac_tables.py <H.264 PDF> > src/cabac/tables.rs
       python scripts/gen_cabac_tables.py <H.264 PDF> --check <h264_cabac.c>
"""
import re
import subprocess
import sys
import xml.etree.ElementTree as ET

# FNV-1a 64 over INIT_I then INIT_PB[0..3], two bytes (m, n as u8) per entry.  Set after
# `--check` reported no mismatch against FFmpeg.
PINNED_FNV1A = 0xF1F713CE3D4B7C86

EDITION = "ITU-T H.264 (V16) (06/2026)"
TYPOS = {("9-26", 641): 541}

# Row layout: tables 9-18 .. 9-33 and the ctxIdx ranges they must give.
ROW_TABLES = {
    "9-18": range(70, 105),
    "9-19": range(105, 166),
    "9-20": range(166, 227),
    "9-21": range(227, 276),
    "9-22": range(277, 338),
    "9-23": range(338, 399),
    "9-24": range(402, 460),
    "9-25": range(460, 484),
    "9-26": range(484, 572),
    "9-27": range(572, 660),
    "9-28": range(660, 718),
    "9-29": range(718, 776),
    "9-30": range(776, 864),
    "9-31": range(864, 952),
    "9-32": range(952, 1012),
    "9-33": range(1012, 1024),
}
# Column layout: (ctxIdx columns, sets of the m/n row pairs in order).  "all" = valid for all
# slice types, "I" = I and SI slices, 0..2 = cabac_init_idc of P, SP and B slices.
COL_TABLES = {
    "9-12": (list(range(0, 11)), ["all"]),
    "9-13": (list(range(11, 24)), [0, 1, 2]),
    "9-14": (list(range(24, 40)), [0, 1, 2]),
    "9-15": (list(range(40, 54)), [0, 1, 2]),
    "9-16": (list(range(54, 60)) + [399, 400, 401], ["I", 0, 1, 2]),
    "9-17": (list(range(60, 70)), ["all"]),
}
NO_CONTEXT = 276

TITLE_RE = re.compile(r"^Table (9-\d+)\s*\S\s*Values of variables m and n for ctxIdx from")
INT_RE = re.compile(r"^-?\d+$")
FOOTER_Y = 795.0


def die(msg):
    sys.exit(f"error: {msg}")


def table_pages(pdf):
    """Map table name -> list of 1-based page numbers carrying its title."""
    text = subprocess.run(["pdftotext", "-layout", pdf, "-"], capture_output=True, check=True).stdout.decode("utf-8", "replace")
    pages = {}
    for no, page in enumerate(text.split("\f"), 1):
        for line in page.splitlines():
            line = line.strip().replace("−", "-")
            m = re.match(r"^Table (9-\d+)\s*\S\s*Values of variables m and n for ctxIdx from", line)
            if m and "...." not in line:
                pages.setdefault(m[1], [])
                if no not in pages[m[1]]:
                    pages[m[1]].append(no)
    return pages


def page_words(pdf, page):
    """(titles, words) of a page; titles = [(name, y)], words = [(x0, x1, y, text)]."""
    xml = subprocess.run(["mutool", "draw", "-F", "stext", "-o", "-", pdf, str(page)], capture_output=True, check=True).stdout
    root = ET.fromstring(xml.decode("utf-8"))
    titles, words = [], []
    for line in root.iter("line"):
        cur = None
        for ch in line.iter("char"):
            q = [float(v) for v in ch.get("quad").split()]
            x0, x1 = min(q[0::2]), max(q[0::2])
            y = (min(q[1::2]) + max(q[1::2])) / 2
            c = ch.get("c").replace("−", "-")
            if c.isspace():
                cur = None
                continue
            if cur is None or x0 - cur[1] > 1.0:
                cur = [x0, x1, y, c]
                words.append(cur)
            else:
                cur[1] = x1
                cur[3] += c
        m = TITLE_RE.match(re.sub(r"\s+", " ", line.get("text", "")))
        if m:
            b = [float(v) for v in line.get("bbox").split()]
            titles.append((m[1], (b[1] + b[3]) / 2))
    return titles, [tuple(w) for w in words]


def regions(pdf, name, pages):
    """Words of the regions of table `name`: its title down to the next title / page footer."""
    out = []
    for p in pages:
        titles, words = page_words(pdf, p)
        titles.sort(key=lambda t: t[1])
        for i, (n, y) in enumerate(titles):
            if n != name:
                continue
            end = titles[i + 1][1] - 5 if i + 1 < len(titles) else FOOTER_Y
            out.append((y, end, [w for w in words if y + 5 < w[2] < end]))
    if not out:
        die(f"Table {name} not found")
    return out


def to_val(s, where):
    if s == "na":
        return None
    if not INT_RE.match(s):
        die(f"{where}: bad number {s!r}")
    return int(s)


def parse_columns(pdf, name, pages, cols, row_sets):
    (ytop, ybot, words), = regions(pdf, name, pages)  # these tables are not split over pages
    # header: the digit words on the line of the last expected ctxIdx column
    last = min((w for w in words if w[3] == str(cols[-1]) and w[0] > 150), key=lambda w: w[2], default=None)
    if last is None:
        die(f"Table {name}: header not found")
    head = sorted((w for w in words if w[3].isdigit() and w[0] > 150 and abs(w[2] - last[2]) < 4), key=lambda w: w[0])
    if [int(w[3]) for w in head] != cols:
        die(f"Table {name}: header columns {[w[3] for w in head]}")
    hx = head[0][0] - 15
    labels = sorted((w for w in words if w[3] in ("m", "n") and w[1] < hx and w[2] > last[2] + 4), key=lambda w: w[2])
    if [w[3] for w in labels] != ["m", "n"] * len(row_sets):
        die(f"Table {name}: row labels {[w[3] for w in labels]}")
    ly = [w[2] for w in labels]
    centres = [(w[0] + w[1]) / 2 for w in head]
    bounds = [y - 6 for y in ly] + [ybot]
    cells = {}
    for w in words:
        if w[0] < hx or w in head or w[2] < bounds[0]:
            continue
        row = max(i for i in range(len(ly)) if w[2] >= bounds[i])
        c = min(range(len(cols)), key=lambda k: abs(centres[k] - (w[0] + w[1]) / 2))
        if abs(centres[c] - (w[0] + w[1]) / 2) > 12:
            die(f"Table {name}: stray word {w}")
        cells.setdefault((row, c), []).append(w)
    res = {}
    for r, kind in enumerate(row_sets):
        mr, nr = 2 * r, 2 * r + 1
        for c, ctx in enumerate(cols):
            vals = []
            for row in (mr, nr):
                frags = sorted(cells.get((row, c), []), key=lambda w: w[2])
                if not 1 <= len(frags) <= 3:
                    die(f"Table {name}: ctxIdx {ctx} set {kind}: {len(frags)} fragments")
                vals.append(to_val("".join(f[3] for f in frags), f"Table {name} ctxIdx {ctx}"))
            res[(kind, ctx)] = tuple(vals)
    if len(cells) != 2 * len(row_sets) * len(cols):
        die(f"Table {name}: {len(cells)} cells, expected {2 * len(row_sets) * len(cols)}")
    return res


def parse_rows(pdf, name, pages, expected):
    res = {}
    for ytop, ybot, words in regions(pdf, name, pages):
        words = sorted(words, key=lambda w: w[2])
        lines, cur = [], []
        for w in words:
            if cur and w[2] - cur[-1][2] > 3.0:
                lines.append(cur)
                cur = []
            cur.append(w)
        if cur:
            lines.append(cur)
        for ln in lines:
            toks = [w[3] for w in sorted(ln, key=lambda w: w[0])]
            if not all(INT_RE.match(t) for t in toks):
                continue
            if len(toks) not in (9, 18):
                die(f"Table {name}: row with {len(toks)} integers: {toks}")
            for g in range(0, len(toks), 9):
                ctx = int(toks[g])
                ctx = TYPOS.get((name, ctx), ctx)
                v = [int(t) for t in toks[g + 1 : g + 9]]
                if ctx in res:
                    die(f"Table {name}: ctxIdx {ctx} twice")
                res[ctx] = {"I": (v[0], v[1]), 0: (v[2], v[3]), 1: (v[4], v[5]), 2: (v[6], v[7])}
    if sorted(res) != list(expected):
        die(f"Table {name}: ctxIdx set differs: missing {sorted(set(expected) - set(res))}, extra {sorted(set(res) - set(expected))}")
    return res


def extract(pdf):
    """-> (I, PB, used_I, used_PB): I[1024] of (m, n), PB[3][1024]; used_* flags."""
    pages = table_pages(pdf)
    want = set(COL_TABLES) | set(ROW_TABLES)
    if set(pages) != want:
        die(f"tables found {sorted(pages)}, expected {sorted(want)}")
    I = [(0, 0)] * 1024
    PB = [[(0, 0)] * 1024 for _ in range(3)]
    uI, uPB = [False] * 1024, [[False] * 1024 for _ in range(3)]
    seen = {}

    def put(table, ctx, kind, mn):
        if mn is None:
            return  # "na"
        if None in mn:
            die(f"Table {table} ctxIdx {ctx}: incomplete {mn}")
        if kind in ("I", "all"):
            if uI[ctx]:
                die(f"ctxIdx {ctx} set twice (I)")
            I[ctx], uI[ctx] = mn, True
        for idc in (0, 1, 2):
            if kind in ("all", idc):
                if uPB[idc][ctx]:
                    die(f"ctxIdx {ctx} set twice (idc {idc})")
                PB[idc][ctx], uPB[idc][ctx] = mn, True
        seen[ctx] = table

    for name, (cols, row_sets) in COL_TABLES.items():
        for (kind, ctx), mn in parse_columns(pdf, name, pages[name], cols, row_sets).items():
            if mn == (None, None):
                mn = None
            put(name, ctx, kind, mn)
    for name, expected in ROW_TABLES.items():
        for ctx, sets in parse_rows(pdf, name, pages[name], expected).items():
            for kind, mn in sets.items():
                put(name, ctx, kind, mn)
    if sorted(seen) != [c for c in range(1024) if c != NO_CONTEXT]:
        die("ctxIdx coverage: " + str(sorted(set(range(1024)) - set(seen))))
    # Table 9-11: I and SI slices use none of 11..=59; the cabac_init_idc sets use all.
    if [c for c in range(1024) if not uI[c]] != list(range(11, 60)) + [NO_CONTEXT]:
        die("unexpected set of unused I entries")
    for idc in range(3):
        if [c for c in range(1024) if not uPB[idc][c]] != [NO_CONTEXT]:
            die(f"unexpected set of unused entries for cabac_init_idc {idc}")
    for ctx in range(1024):
        for t in [I[ctx]] + [PB[i][ctx] for i in range(3)]:
            if not (-128 <= t[0] <= 127 and -128 <= t[1] <= 127):
                die(f"ctxIdx {ctx}: {t} out of i8")
    return I, PB, uI, uPB


def fnv1a(I, PB):
    h = 0xCBF29CE484222325
    for table in [I] + PB:
        for m, n in table:
            for b in (m & 0xFF, n & 0xFF):
                h = ((h ^ b) * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return h


def ffmpeg_tables(path):
    src = open(path, encoding="utf-8").read()
    src = re.sub(r"/\*.*?\*/", "", src, flags=re.S)

    def body(name):
        m = re.search(r"\b" + name + r"\b[^=]*=\s*\{(.*?)\n\};", src, re.S)
        if not m:
            die(f"{name} not found in {path}")
        return [(int(a), int(b)) for a, b in re.findall(r"\{\s*(-?\d+)\s*,\s*(-?\d+)\s*\}", m[1])]

    i = body("cabac_context_init_I")
    pb = body("cabac_context_init_PB")
    if len(i) != 1024 or len(pb) != 3 * 1024:
        die(f"FFmpeg tables have {len(i)} / {len(pb)} entries")
    return i, [pb[k * 1024 : (k + 1) * 1024] for k in range(3)]


def check(pdf, path):
    I, PB, uI, uPB = extract(pdf)
    fi, fpb = ffmpeg_tables(path)
    compared = bad = 0
    info = {}
    sets = [("I", I, fi, uI)] + [(f"idc{k}", PB[k], fpb[k], uPB[k]) for k in range(3)]
    for label, ours, theirs, used in sets:
        for ctx in range(1024):
            if used[ctx]:
                compared += 1
                if ours[ctx] != theirs[ctx]:
                    bad += 1
                    print(f"MISMATCH {label} ctxIdx {ctx}: spec {ours[ctx]} FFmpeg {theirs[ctx]}")
            elif theirs[ctx] != (0, 0):
                info.setdefault(label, []).append(ctx)
    print(f"compared {compared} entries defined by the standard, {bad} mismatches")
    for label, ctxs in info.items():
        print(f"info: {label}: FFmpeg has non-zero values at {len(ctxs)} entries the standard marks unused: {ctxs}")
    h = fnv1a(I, PB)
    print(f"FNV-1a 64 of the extracted table: 0x{h:016X} (pinned: 0x{PINNED_FNV1A:016X})")
    if bad:
        sys.exit(1)
    if h != PINNED_FNV1A:
        print("note: the pin differs; set PINNED_FNV1A to the value above")
        sys.exit(1)
    print("OK")


def pairs_block(table):
    out = []
    for base in range(0, 1024, 8):
        row = ", ".join(f"({m}, {n})" for m, n in table[base : base + 8])
        out.append(f"    /* {base} */ {row},")
    return out


TESTS = r'''
#[cfg(test)]
mod tests {
    use super::*;

    /// Entry of Tables 9-12..9-33 typed by hand from the standard: I (None = not used) and
    /// cabac_init_idc 0, 1, 2.
    fn spot(ctx: usize, i: Option<(i8, i8)>, pb: [(i8, i8); 3]) {
        assert_eq!(INIT_I[ctx], i.unwrap_or((0, 0)), "I ctxIdx {ctx}");
        for (idc, want) in pb.into_iter().enumerate() {
            assert_eq!(INIT_PB[idc][ctx], want, "cabac_init_idc {idc} ctxIdx {ctx}");
        }
    }

@SPOTS@
    #[test]
    fn unused_entries_are_zero() {
        assert!(INIT_I[11..=59].iter().all(|&e| e == (0, 0)));
        for set in std::iter::once(&INIT_I).chain(INIT_PB.iter()) {
            assert_eq!(set[276], (0, 0));
        }
    }

    #[test]
    fn used_entries_are_in_range() {
        for (name, set) in std::iter::once(("I", &INIT_I)).chain(INIT_PB.iter().map(|s| ("PB", s))) {
            for (ctx, &(m, n)) in set.iter().enumerate() {
                let used = ctx != 276 && !(name == "I" && (11..=59).contains(&ctx));
                if used {
                    assert!((@M_MIN@..=@M_MAX@).contains(&m), "{name} ctxIdx {ctx}: m = {m}");
                    assert!((@N_MIN@..=@N_MAX@).contains(&n), "{name} ctxIdx {ctx}: n = {n}");
                }
            }
        }
    }

    /// FNV-1a 64 over the (m, n) bytes of INIT_I then INIT_PB[0..3].  The expected value is the
    /// one printed by `gen_cabac_tables.py --check` after the table extracted from the standard
    /// had matched FFmpeg's on every entry the standard defines; it catches accidental edits.
    #[test]
    fn fingerprint() {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for set in std::iter::once(&INIT_I).chain(INIT_PB.iter()) {
            for &(m, n) in set.iter() {
                for b in [m as u8, n as u8] {
                    h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
                }
            }
        }
        assert_eq!(h, @HASH@);
    }
}
'''
# Hand-typed from the standard: ctxIdx, I, idc0, idc1, idc2 (all-slice tables repeat one pair)
SPOTS = {
    "9-12": [(0, (20, -15)), (6, (-28, 127)), (9, (-1, 54)), (10, (7, 51))],
    "9-13": [(11, None, (23, 33), (22, 25), (29, 16)), (16, None, (-37, 118), (-29, 118), (-27, 99)), (23, None, (17, 50), (10, 54), (14, 57))],
    "9-14": [  # ctxIdx 30, 34 and 37 are wrapped cells in the PDF
        (30, None, (-46, 127), (-45, 127), (-32, 127)),
        (34, None, (-11, 65), (-13, 79), (-24, 102)),
        (37, None, (-17, 95), (-13, 90), (-14, 88)),
    ],
    "9-15": [(40, None, (-3, 69), (-2, 69), (-11, 89)), (46, None, (2, 88), (-3, 100), (6, 96)), (53, None, (0, 88), (-5, 95), (-1, 101))],
    "9-16": [
        (54, None, (-7, 67), (-1, 66), (3, 55)),
        (57, None, (-5, 80), (-2, 86), (-12, 97)),
        (399, (31, 21), (12, 40), (25, 32), (21, 33)),
        (401, (25, 50), (14, 59), (21, 54), (17, 61)),
    ],
    "9-17": [(60, (0, 41)), (64, (-9, 83)), (65, (4, 86)), (68, (13, 41)), (69, (3, 62))],
    "9-18": [(70, (0, 11), (0, 45), (13, 15), (7, 34)), (73, (-17, 127), (-27, 126), (-39, 127), (-36, 127)), (88, (-11, 115), (-13, 108), (-4, 92), (5, 78))],
    "9-19": [(105, (-7, 93), (-2, 85), (-13, 103), (-4, 86)), (135, (-15, 100), (2, 53), (-5, 61), (-15, 72)), (136, (-13, 101), (5, 53), (0, 58), (-5, 75))],
    "9-20": [(166, (24, 0), (11, 28), (4, 45), (4, 39)), (167, (15, 9), (2, 40), (10, 28), (0, 42)), (197, (26, -17), (28, 3), (36, -28), (28, -3))],
    "9-21": [(227, (-3, 71), (-6, 76), (-23, 112), (-24, 115)), (250, (-6, 62), (1, 38), (-10, 57), (-8, 66)), (275, (-14, 97), (-8, 85), (-4, 78), (-10, 87))],
    "9-22": [(277, (-6, 93), (-13, 106), (-21, 126), (-22, 127)), (300, (18, 32), (9, 58), (5, 64), (0, 71)), (331, (3, 62), (13, 40), (12, 41), (5, 54))],
    "9-23": [(338, (15, 6), (14, 11), (19, -6), (17, -13)), (370, (37, -30), (27, 6), (38, -30), (32, 1)), (398, (19, 66), (11, 86), (11, 83), (25, 61))],
    "9-24": [(402, (-17, 120), (-4, 79), (-5, 85), (-3, 78)), (430, (0, 54), (-5, 48), (-2, 52), (-10, 56)), (459, (14, 67), (42, 62), (47, 57), (20, 64))],
    "9-25": [(460, (-17, 123), (-7, 92), (0, 80), (11, 80)), (461, (-12, 115), (-5, 89), (-5, 89), (5, 76)), (472, (-17, 123), (-7, 92), (0, 80), (11, 80))],
    "9-26": [  # 541 is misprinted 641 in the PDF
        (484, (-7, 93), (-2, 85), (-13, 103), (-4, 86)),
        (495, (-2, 66), (-4, 70), (-78, 127), (-4, 71)),
        (541, (0, 61), (2, 57), (-50, 127), (2, 58)),
        (528, (-7, 93), (-2, 85), (-13, 103), (-4, 86)),
    ],
    "9-27": [(572, (24, 0), (11, 28), (4, 45), (4, 39)), (580, (6, 29), (4, 39), (38, -25), (8, 33)), (659, (41, 17), (17, 70), (29, 41), (22, 73))],
    "9-28": [(660, (-17, 120), (-4, 79), (-5, 85), (-3, 78)), (700, (24, -11), (33, -14), (32, -13), (30, -10)), (717, (-9, 92), (-8, 76), (-7, 82), (-10, 79))],
    "9-29": [(718, (-17, 120), (-4, 79), (-5, 85), (-3, 78)), (731, (-24, 50), (-22, 65), (0, 59), (-9, 52)), (760, (28, -1), (46, -2), (49, -5), (33, -1))],
    "9-30": [(776, (-6, 93), (-13, 106), (-21, 126), (-22, 127)), (800, (5, 42), (5, 61), (1, 67), (-1, 73)), (863, (-1, 83), (6, 64), (-2, 75), (2, 70))],
    "9-31": [(864, (15, 6), (14, 11), (19, -6), (17, -13)), (900, (40, -15), (33, 19), (49, -29), (34, 22)), (908, (15, 6), (14, 11), (19, -6), (17, -13))],
    "9-32": [(952, (-3, 71), (-6, 76), (-23, 112), (-24, 115)), (960, (-1, 74), (-8, 82), (-10, 87), (-11, 92)), (990, (-1, 74), (-8, 82), (-10, 87), (-11, 92))],
    "9-33": [(1012, (-3, 70), (-3, 74), (-2, 73), (-5, 79)), (1018, (-10, 90), (-8, 87), (-9, 91), (-11, 91)), (1023, (-30, 127), (-23, 126), (-31, 127), (-30, 127))],
}


def spot_tests():
    """Rust test functions for SPOTS (typed in by hand from the standard, see above)."""
    out = []
    for table, entries in SPOTS.items():
        out.append("    #[test]")
        out.append(f"    fn table_{table.replace('-', '_')}() {{")
        out.append(f"        // Table {table}")
        for e in entries:
            if len(e) == 2:  # valid for all slice types
                ctx, mn = e
                e = (ctx, mn, mn, mn, mn)
            ctx, i, *pb = e
            iv = "None" if i is None else f"Some({i})"
            out.append(f"        spot({ctx}, {iv}, [{', '.join(str(p) for p in pb)}]);")
        out.append("    }")
        out.append("")
    return chr(10).join(out)


def generate(pdf):
    I, PB, uI, uPB = extract(pdf)
    h = fnv1a(I, PB)
    if h != PINNED_FNV1A:
        die(f"table hash 0x{h:016X} differs from PINNED_FNV1A 0x{PINNED_FNV1A:016X}: run --check and review")
    flat = [t[c] for t, u in zip([I] + PB, [uI] + uPB) for c in range(1024) if u[c]]
    tests = (
        TESTS.replace("@SPOTS@", spot_tests())
        .replace("@M_MIN@", str(min(m for m, _ in flat)))
        .replace("@M_MAX@", str(max(m for m, _ in flat)))
        .replace("@N_MIN@", str(min(n for _, n in flat)))
        .replace("@N_MAX@", str(max(n for _, n in flat)))
        .replace("@HASH@", f"0x{h:016x}")
    )
    o = []
    w = o.append
    w("//! CABAC context initialisation values (m, n) of ITU-T H.264 Tables 9-12 to 9-33 (clause 9.3.1.1).")
    w(f"//! Generated by scripts/gen_cabac_tables.py from {EDITION}; do not edit.")
    w("//!")
    w("//! Entries a slice type does not use are (0, 0): ctxIdx 11..=59 for I and SI slices (Table 9-11), and")
    w("//! ctxIdx 276 everywhere (end_of_slice_flag / I_PCM bin: the terminate process has no context variable).")
    w("")
    w("/// (m, n) for I and SI slices, indexed by ctxIdx")
    w("#[rustfmt::skip]")
    w("pub const INIT_I: [(i8, i8); 1024] = [")
    o.extend(pairs_block(I))
    w("];")
    w("")
    w("/// (m, n) for P, SP and B slices, indexed by cabac_init_idc and ctxIdx")
    w("#[rustfmt::skip]")
    w("pub const INIT_PB: [[(i8, i8); 1024]; 3] = [")
    for k in range(3):
        w("    [")
        o.extend("    " + line for line in pairs_block(PB[k]))
        w("    ],")
    w("];")
    sys.stdout.buffer.write(("\n".join(o) + "\n" + tests).encode("ascii"))


def main():
    args = sys.argv[1:]
    if len(args) == 1:
        generate(args[0])
    elif len(args) == 3 and args[1] == "--check":
        check(args[0], args[2])
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
