#!/usr/bin/env python3
"""Differential fuzz of REAL->text between turso_core and real sqlite3.

usage: fuzz.py <count> [seed]   (run from the repo root, inside the dev rootfs)
"""
import os, random, sqlite3, struct, subprocess, sys

n = int(sys.argv[1])
rnd = random.Random(int(sys.argv[2]) if len(sys.argv) > 2 else 1)

def bits(f):
    return struct.unpack("<Q", struct.pack("<d", f))[0]

vals = []
def add(f):
    if f == f and abs(f) != float("inf"):
        vals.append(bits(f))
        vals.append(bits(-f))
for e in range(-1074, 1024):
    f = 2.0 ** e
    add(f)
    add(struct.unpack("<d", struct.pack("<Q", bits(f) + 1))[0] if e < 1023 else f)
for e in range(-323, 309):
    for m in (1, 5, 9, 15, 25, 125):
        try:
            add(float(f"{m}e{e}"))
        except OverflowError:
            pass
for s in ("1025.1655084065987", "0.1", "49.47", "0.30000000000000004", "9.5", "0.5", "2.5", "1e15", "1e16", "1e17", "123456789012345.6", "999999999999999.9", "9.999999999999999e22"):
    add(float(s))
while len(vals) < n:
    k = rnd.random()
    if k < 0.3:
        b = rnd.getrandbits(64)
    elif k < 0.5:  # subnormal
        b = rnd.getrandbits(52)
    elif k < 0.7:  # short decimal
        f = float(f"{rnd.randint(1, 10**rnd.randint(1, 17))}e{rnd.randint(-30, 30)}")
        b = bits(f)
    elif k < 0.85:  # near neighbours of short decimals
        f = float(f"{rnd.randint(1, 10**rnd.randint(1, 17))}e{rnd.randint(-30, 30)}")
        b = bits(f) + rnd.randint(-3, 3)
    else:  # moderate range
        b = bits(rnd.uniform(-1e6, 1e6) * 10 ** rnd.randint(-8, 8))
    b &= (1 << 64) - 1
    f = struct.unpack("<d", struct.pack("<Q", b))[0]
    if f == f and abs(f) != float("inf"):
        vals.append(b)

work = os.environ.get("FLOAT_WORK", "/src/target/fdwork")
os.makedirs(work, exist_ok=True)
inp, out = f"{work}/float_bits.txt", f"{work}/float_out.txt"
with open(inp, "w") as fh:
    fh.write("\n".join(f"{b:x}" for b in vals) + "\n")
env = dict(os.environ, FLOAT_BITS=inp, FLOAT_OUT=out)
subprocess.run(["cargo", "test", "-p", "turso_core", "--lib", "float_digits_dump", "--", "--ignored", "--nocapture"], check=True, env=env, stdout=subprocess.DEVNULL)

db = sqlite3.connect(":memory:")
print("sqlite", sqlite3.sqlite_version, "values", len(vals))
bad = 0
with open(out) as fh:
    for line in fh:
        h, t, g15, g17, g17p, q = line.rstrip("\n").split("\t")
        x = struct.unpack("<d", struct.pack("<Q", int(h, 16)))[0]
        row = db.execute(
            "SELECT CAST(?1 AS TEXT), printf('%!.15g',?1), printf('%.17g',?1), quote(?1), json_quote(?1), ?1||'', json_array(?1)",
            (x,),
        ).fetchone()
        want_g17 = db.execute("SELECT printf('%!.17g',?1)", (x,)).fetchone()[0]
        got = (t, g15, g17, g17p, q)
        exp = (row[0], row[1], want_g17, row[2], row[3])
        if got != exp or row[4] != row[0] or row[5] != row[0] or row[6] != "[" + row[0] + "]":
            bad += 1
            if bad < 30:
                print("MISMATCH", h, x, "turso", got, "sqlite", row, want_g17)
print("mismatches:", bad)

# text -> REAL
strs = []
for _ in range(max(n // 4, 1000)):
    d = rnd.randint(1, 10 ** rnd.randint(1, 19))
    form = rnd.random()
    if form < 0.5:
        strs.append(f"{d}e{rnd.randint(-340, 310)}")
    elif form < 0.8:
        s = str(d)
        k = rnd.randint(0, len(s))
        strs.append(f"{s[:k] or '0'}.{s[k:]}e{rnd.randint(-30, 30)}")
    else:
        strs.append(f"{d}")
sin, sout = f"{work}/float_strs.txt", f"{work}/float_parse.txt"
with open(sin, "w") as fh:
    fh.write("\n".join(strs) + "\n")
env = dict(os.environ, FLOAT_STRS=sin, FLOAT_PARSE_OUT=sout)
subprocess.run(["cargo", "test", "-p", "turso_core", "--lib", "float_parse_dump", "--", "--ignored"], check=True, env=env, stdout=subprocess.DEVNULL)
pbad = 0
with open(sout) as fh:
    for st, h in zip(strs, fh):
        want = db.execute("SELECT CAST(? AS REAL)", (st,)).fetchone()[0]
        if bits(want) != int(h, 16):
            pbad += 1
            if pbad < 20:
                print("PARSE MISMATCH", st, "turso", h.strip(), "sqlite", f"{bits(want):x}")
print("parse mismatches:", pbad)
bad += pbad
sys.exit(1 if bad else 0)
