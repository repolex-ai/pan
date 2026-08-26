#!/usr/bin/env python3
"""Corpus-wide XMP shape survey of a Pool store — read-only.

Answers, for every image (or a sample), the questions that decide how much
hand-repair a real migration needs:
  - does the packet declare a prefix only on the root Description while SIBLING
    Descriptions use it? (the malformation that makes strict parsers refuse)
  - which caption fields exist, and therefore which models are represented?
  - which images carry regions / poses / vectors at all?
  - what field names exist per month?
"""
import os
import re
import sys
import zlib
import struct
import json
from collections import Counter, defaultdict

POOL = sys.argv[1] if len(sys.argv) > 1 else "/Volumes/p02/_copia/pool"
STRIDE = int(sys.argv[2]) if len(sys.argv) > 2 else 1

XMP_KEY = b"XML:com.adobe.xmp"


def read_xmp(path):
    """Pull the XMP packet out of a PNG without decoding pixels."""
    try:
        with open(path, "rb") as f:
            if f.read(8) != b"\x89PNG\r\n\x1a\n":
                return None
            while True:
                head = f.read(8)
                if len(head) < 8:
                    return None
                length, ctype = struct.unpack(">I4s", head)
                if ctype in (b"IDAT", b"IEND"):
                    return None
                data = f.read(length)
                f.read(4)  # crc
                if ctype == b"iTXt" and data.startswith(XMP_KEY):
                    rest = data[len(XMP_KEY) + 1:]
                    comp_flag = rest[0]
                    rest = rest[1:]
                    # comp method, lang, translated keyword: three NUL-terminated
                    rest = rest[1:]
                    for _ in range(2):
                        idx = rest.index(b"\x00")
                        rest = rest[idx + 1:]
                    if comp_flag:
                        rest = zlib.decompress(rest)
                    return rest.decode("utf-8", "replace")
                if ctype == b"tEXt" and data.startswith(XMP_KEY):
                    return data[len(XMP_KEY) + 1:].decode("latin-1", "replace")
    except Exception:
        return None
    return None


desc_re = re.compile(r"<rdf:Description\b([^>]*)>")
prefix_use_re = re.compile(r"<([A-Za-z0-9_-]+):")
xmlns_re = re.compile(r'xmlns:([A-Za-z0-9_-]+)\s*=')

stats = Counter()
by_month_fields = defaultdict(Counter)
caption_fields = Counter()
month_counts = Counter()
malformed_by_month = Counter()
region_counts = []

blob_root = os.path.join(POOL, "blob/image")
files = []
for year in sorted(os.listdir(blob_root)):
    if not year.isdigit():
        continue
    ydir = os.path.join(blob_root, year)
    for month in sorted(os.listdir(ydir)):
        mdir = os.path.join(ydir, month)
        if not os.path.isdir(mdir):
            continue
        for day in sorted(os.listdir(mdir)):
            ddir = os.path.join(mdir, day)
            if not os.path.isdir(ddir):
                continue
            for fn in sorted(os.listdir(ddir)):
                if fn.endswith(".png"):
                    files.append((f"{year}/{month}", os.path.join(ddir, fn)))

files = files[::STRIDE]
print(f"scanning {len(files)} images (stride {STRIDE})", file=sys.stderr)

for i, (month, path) in enumerate(files):
    if i % 2000 == 0 and i:
        print(f"  {i}/{len(files)}", file=sys.stderr)
    month_counts[month] += 1
    packet = read_xmp(path)
    if packet is None:
        stats["no_xmp"] += 1
        continue
    stats["with_xmp"] += 1

    descs = desc_re.findall(packet)
    # Root = the first Description; siblings = the rest.
    root_attrs = descs[0] if descs else ""
    root_ns = set(xmlns_re.findall(root_attrs))
    sibling_attrs = descs[1:]
    stats["has_siblings"] += 1 if sibling_attrs else 0

    if sibling_attrs:
        # Which prefixes do sibling elements USE, and do they declare them?
        # Split the packet at the end of the root Description.
        root_close = packet.find("</rdf:Description>")
        tail = packet[root_close:] if root_close > 0 else ""
        used = set(prefix_use_re.findall(tail)) - {"rdf", "x"}
        declared_in_tail = set(xmlns_re.findall(tail))
        undeclared = used - declared_in_tail
        if undeclared:
            stats["sibling_prefix_out_of_scope"] += 1
            malformed_by_month[month] += 1
            for p in undeclared:
                stats[f"oos_prefix::{p}"] += 1

    for m in re.finditer(r"<copia:([A-Za-z0-9]*[Cc]aption)>", packet):
        caption_fields[m.group(1)] += 1
    n_regions = packet.count('rdf:about="Sam3Region:')
    if n_regions:
        region_counts.append(n_regions)
        stats["with_regions"] += 1
    if 'rdf:about="PoseDetection:' in packet:
        stats["with_poses"] += 1
    for m in re.finditer(r"<([A-Za-z0-9_-]+):([A-Za-z0-9_]+)>", packet):
        if m.group(1) not in ("rdf", "x"):
            by_month_fields[month][f"{m.group(1)}:{m.group(2)}"] += 1

out = {
    "scanned": len(files),
    "stats": dict(stats),
    "caption_fields": dict(caption_fields),
    "month_counts": dict(month_counts),
    "malformed_by_month": dict(malformed_by_month),
    "regions": {
        "images_with_regions": len(region_counts),
        "total_regions": sum(region_counts),
        "max_regions_in_one_image": max(region_counts) if region_counts else 0,
    },
    "fields_by_month": {m: dict(c.most_common()) for m, c in by_month_fields.items()},
}
json.dump(out, open("corpus_scan.json", "w"), indent=1)
print(json.dumps({k: v for k, v in out.items() if k != "fields_by_month"}, indent=1))
