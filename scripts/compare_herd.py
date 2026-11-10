#!/usr/bin/env python3
"""Compare armem's ARM model against herd7 running Arm's official aarch64.cat.

For each litmus test, both tools enumerate the allowed final states; the
script reports tests where the state sets differ.

usage: compare_herd.py [options] FILE.litmus...

Tools are found via --herd/--libdir, or the HERD7 / HERDLIB environment
variables, or ~/.local/opt/herdtools7 (see scripts/install_herdtools.sh).
"""

import argparse
import concurrent.futures
import os
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_HERD = Path.home() / ".local/opt/herdtools7/usr/bin/herd7"
DEFAULT_LIBDIR = Path.home() / ".local/opt/herdtools7-src/herd/libdir"
DEFAULT_ARMEM = ROOT / "target/release/armem"
# Tests where armem deliberately differs from herd7's aarch64.cat, with the
# reason. They are reported as KNOWN-DIFF instead of MISMATCH.
KNOWN_DIFFERENCES = {
    "LDXP-torn": (
        "herd7 treats an X-register LDXP as a 128-bit single-copy-atomic read. "
        "The Arm ARM only guarantees that when a matching STXP succeeds "
        "(GCC and LLVM rely on this), so armem lets a lone LDXP be torn."
    ),
    "ALU-extend-shift": (
        "herd7 shifts a UXTB/UXTH/SXTB/SXTH extended register operand before "
        "extending it (ext_sext in herd/AArch64Sem.ml), so it extends from "
        "bit len-1 of the shifted value. The Arm ARM's ExtendReg extends "
        "val<len-1:0> : Zeros(shift), from bit len+shift-1, as armem does. "
        "herd7 also sign-extends to 64 bits in a W-register operation, and "
        "does not truncate the result: ADD W10,W1,W0,SXTH gives a negative "
        "X10, where a W-register write clears the upper 32 bits. "
        "Reported as https://github.com/herd/herdtools7/issues/2107."
    ),
}

LOOP = re.compile(r"^\s*(B|B\.\w+|CBN?Z|TBN?Z)\b", re.I | re.M)


def norm_value(v):
    """Values as integers modulo 2^64; symbols (addresses) as names."""
    v = v.strip()
    try:
        return int(v, 0) % (1 << 64)
    except ValueError:
        return v


def norm_key(k):
    k = k.strip()
    m = re.fullmatch(r"\[(\w+)\]", k)
    return m.group(1) if m else k


def parse_state(line):
    state = {}
    for item in line.split(";"):
        item = item.strip()
        if not item:
            continue
        k, _, v = item.partition("=")
        state[norm_key(k)] = norm_value(v)
    return frozenset(state.items())


def parse_states(text, header_re):
    """Extract the state set following a 'States N' line in a section."""
    lines = text.splitlines()
    for i, l in enumerate(lines):
        if re.match(header_re, l):
            for j in range(i + 1, len(lines)):
                m = re.match(r"States (\d+)", lines[j])
                if m:
                    n = int(m.group(1))
                    out = set()
                    for s in lines[j + 1 : j + 1 + n]:
                        s = s.split("  [")[0].split("  <--")[0]
                        out.add(parse_state(s))
                    return out
            break
    return None


def run(cmd, timeout):
    try:
        p = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
        return p.stdout + p.stderr, None
    except subprocess.TimeoutExpired:
        return "", "timeout"


def herd_states(args, path, mixed):
    cmd = [str(args.herd), "-set-libdir", str(args.libdir)]
    if mixed:
        cmd += ["-variant", "mixed"]
    if args.unroll is not None:
        cmd += ["-unroll", str(args.unroll)]
    out, err = run(cmd + [str(path)], args.timeout)
    if err:
        return None, err
    states = parse_states(out, r"Test ")
    if states is None:
        msg = next((l for l in out.splitlines() if l.strip()), "no output")
        return None, msg.strip()
    return states, None


def armem_states(args, path):
    cmd = [str(args.armem), "-m", "arm", "-f", str(args.fuel), str(path)]
    if args.lse2:
        cmd.insert(1, "--lse2")
    out, err = run(cmd, args.timeout)
    if err:
        return None, err
    states = parse_states(out, r"Test .* \(ARM\)")
    if states is None:
        return None, out.strip().splitlines()[0] if out.strip() else "no output"
    return states, None


def needs_mixed(src):
    return bool(re.search(r"\b(u?int(8|16|32)_t|short|char)\b", src)) or bool(
        re.search(r"\b(LD|ST)\w*[BH]\b|\bLD[A-Z]*P\b|\bST[A-Z]*P\b|\[X\d+,#\d*[1-79]\]", src, re.I)
    )


def compare(args, path):
    src = path.read_text()
    has_loop = bool(LOOP.search(src))
    mixed = args.mixed or (args.auto_mixed and needs_mixed(src))
    h, herr = herd_states(args, path, mixed)
    a, aerr = armem_states(args, path)
    name = path.stem + (" [mixed]" if mixed else "")
    if herr:
        return name, "HERD-ERROR", herr, None, None
    if aerr:
        return name, "ARMEM-ERROR", aerr, None, None
    only_a = a - h
    only_h = h - a
    if not only_a and not only_h:
        return name, "MATCH", f"{len(a)} states", None, None
    if path.stem in KNOWN_DIFFERENCES:
        return name, "KNOWN-DIFF", KNOWN_DIFFERENCES[path.stem], only_a, only_h
    if has_loop and not only_h:
        # herd bounds loops by unrolling, so it may legitimately see fewer states.
        return name, "SUPERSET", f"armem has {len(only_a)} extra (loop test; herd unrolls)", only_a, None
    return name, "MISMATCH", "", only_a, only_h


def fmt(state):
    return " ".join(f"{k}={v};" for k, v in sorted(state, key=lambda kv: kv[0]))


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("files", nargs="+", type=Path)
    ap.add_argument("--herd", type=Path, default=Path(os.environ.get("HERD7", DEFAULT_HERD)))
    ap.add_argument("--libdir", type=Path, default=Path(os.environ.get("HERDLIB", DEFAULT_LIBDIR)))
    ap.add_argument("--armem", type=Path, default=DEFAULT_ARMEM)
    ap.add_argument("--mixed", action="store_true", help="always run herd7 with -variant mixed")
    ap.add_argument(
        "--no-auto-mixed", dest="auto_mixed", action="store_false",
        help="do not use -variant mixed automatically for tests with sized/pair accesses",
    )
    ap.add_argument("--lse2", action="store_true", help="run armem with --lse2")
    ap.add_argument("--unroll", type=int, help="herd7 loop unrolling bound")
    ap.add_argument("--fuel", type=int, default=200)
    ap.add_argument("--timeout", type=int, default=120, help="per-tool timeout in seconds")
    ap.add_argument("-j", "--jobs", type=int, default=os.cpu_count() or 1)
    ap.add_argument("-q", "--quiet", action="store_true", help="only print non-matching tests")
    args = ap.parse_args()

    for tool in (args.herd, args.armem):
        if not tool.exists():
            sys.exit(f"not found: {tool}")

    with concurrent.futures.ThreadPoolExecutor(args.jobs) as ex:
        results = list(ex.map(lambda p: compare(args, p), args.files))

    counts = {}
    for name, status, msg, only_a, only_h in results:
        counts[status] = counts.get(status, 0) + 1
        if args.quiet and status in ("MATCH", "SUPERSET", "KNOWN-DIFF"):
            continue
        print(f"{status:<12} {name}  {msg}")
        for s in sorted(only_a or [], key=fmt):
            print(f"    only armem: {fmt(s)}")
        for s in sorted(only_h or [], key=fmt):
            print(f"    only herd:  {fmt(s)}")
    print()
    print("Summary: " + ", ".join(f"{k} {v}" for k, v in sorted(counts.items())))
    sys.exit(1 if counts.get("MISMATCH") or counts.get("ARMEM-ERROR") else 0)


if __name__ == "__main__":
    main()
