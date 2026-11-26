#!/usr/bin/env python3
# SPDX-License-Identifier: BSD-2-Clause
"""Compares glmark2 runs on AbyssBSD with runs on FreeBSD (drm-kmod) on the
same board: per test, the median frames per second of each side's runs, the
ratio, and the geometric mean of the ratios (above 1 is faster than FreeBSD).

Usage: compare-glmark2.py [DIR [ABYSS-PREFIX [FREEBSD-PREFIX]]]
DIR defaults to `results` beside this file; the prefixes to `abyss-gl` and
`fbsd-gl` (the files are PREFIX-1.txt, PREFIX-2.txt, ...).
"""
import glob
import math
import os
import re
import statistics
import sys


def load(pattern):
    runs = []
    for name in sorted(glob.glob(pattern)):
        run = {}
        with open(name) as f:
            for line in f:
                m = re.match(r"\[(\w+)\] (.*?): FPS: (\d+)", line)
                if m:
                    run[m.group(1) + " " + m.group(2)[:50]] = int(m.group(3))
        runs.append(run)
    return runs


def score(pattern):
    out = []
    for name in sorted(glob.glob(pattern)):
        with open(name) as f:
            for line in f:
                m = re.search(r"glmark2 Score: (\d+)", line)
                if m:
                    out.append(int(m.group(1)))
    return out


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    d = sys.argv[1] if len(sys.argv) > 1 else os.path.join(here, "results")
    a_prefix = sys.argv[2] if len(sys.argv) > 2 else "abyss-gl"
    f_prefix = sys.argv[3] if len(sys.argv) > 3 else "fbsd-gl"
    a = load(os.path.join(d, a_prefix + "-*.txt"))
    f = load(os.path.join(d, f_prefix + "-*.txt"))
    keys = [k for k in a[0] if all(k in r for r in a + f)]
    ratios = []
    for k in keys:
        ma = statistics.median(r[k] for r in a)
        mf = statistics.median(r[k] for r in f)
        ratios.append((ma / mf, k, ma, mf))
    print(f"{'ratio':>5} {'abyss':>6} {'fbsd':>6}  test")
    for r, k, ma, mf in sorted(ratios):
        print(f"{r:5.2f} {ma:6.0f} {mf:6.0f}  {k}")
    geo = math.exp(sum(math.log(r[0]) for r in ratios) / len(ratios))
    print(f"geometric mean of the ratios: {geo:.3f} over {len(ratios)} tests")
    for name, p in (("abyss", a_prefix), ("freebsd", f_prefix)):
        s = score(os.path.join(d, p + "-*.txt"))
        print(f"{name} scores {s}, mean {sum(s) / len(s):.0f}")


main()
