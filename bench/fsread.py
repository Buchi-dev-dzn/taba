"""Read speed of a directory tree: walk+stat+read many small files, and one big file.

Usage: python3 fsread.py DIR   (DIR must contain small/ and big.bin)
Prints medians of 5 passes.
"""

import os
import statistics
import sys
import time

root = sys.argv[1]


def small():
    t = time.perf_counter()
    for dp, _, files in os.walk(os.path.join(root, "small")):
        for f in files:
            p = os.path.join(dp, f)
            os.stat(p)
            with open(p, "rb") as fh:
                fh.read()
    return time.perf_counter() - t


def big():
    t = time.perf_counter()
    with open(os.path.join(root, "big.bin"), "rb") as fh:
        while fh.read(1 << 20):
            pass
    return time.perf_counter() - t


s = statistics.median(small() for _ in range(5))
b = statistics.median(big() for _ in range(5))
print(f"small {s * 1000:7.1f} ms   big {b * 1000:7.1f} ms ({64 / b:6.0f} MiB/s)")
