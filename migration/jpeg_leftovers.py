#!/usr/bin/env python3
"""The 66 Pool files that were JPEG bytes under a .png name (ruled by goodlux,
2026-10-03): 28 distinct images, the rest byte-identical repeats. Each
distinct one is decoded and written as a real PNG (pixels unchanged), given
a pan Description with the ORIGINAL file's creation time and the set
loose-ends, and stored. Repeats are moved to the exported tree unstored.
Rows go to the shared mapping.csv against the original paths."""
import csv, datetime as dt, hashlib, os, shutil, subprocess, sys
sys.path.insert(0, os.path.dirname(__file__))
import pool_to_pan as m

ARCHIVE = "/Volumes/p02/_pan_migration"
SRC_ROOT = "/Volumes/p02/_copia/pool/blob/image"
EXP_ROOT = "/Volumes/p02/_copia_exported/pool/blob/image"
STORE, SET, PAND = "700c5b", "loose-ends", "http://127.0.0.1:7401"
staging = sys.argv[1]

rows = [r for r in csv.DictReader(open(os.path.join(ARCHIVE, "mapping.csv"))) if r["reason"].startswith("not a PNG")]
paths = sorted({r["source_path"] for r in rows if os.path.exists(r["source_path"])})
groups = {}
for p in paths:
    groups.setdefault(hashlib.sha256(open(p, "rb").read()).hexdigest(), []).append(p)
print("files %d, distinct %d" % (len(paths), len(groups)), flush=True)

out = open(os.path.join(ARCHIVE, "mapping.csv"), "a", newline="")
w = csv.DictWriter(out, fieldnames=m.COLUMNS)
def row(status, path, **kw):
    r = {c: "" for c in m.COLUMNS}
    r.update(when=dt.datetime.now().astimezone().isoformat(timespec="seconds"), status=status, source_path=path,
             stem=os.path.splitext(os.path.basename(path))[0], store=STORE, old_in_set_id=SET)
    r.update(kw); w.writerow(r); out.flush()
def export(path):
    dst = os.path.join(EXP_ROOT, os.path.relpath(path, SRC_ROOT))
    os.makedirs(os.path.dirname(dst), exist_ok=True); shutil.move(path, dst)

counts = {}
for digest, ps in sorted(groups.items(), key=lambda kv: kv[1][0]):
    first, rest = ps[0], ps[1:]
    stem = os.path.splitext(os.path.basename(first))[0]
    media_created = m.date_from_birthtime(first)
    png = os.path.join(staging, stem + ".png")
    subprocess.run(["sips", "-s", "format", "png", first, "--out", png], check=True, capture_output=True)
    try:
        from PIL import Image
        a, b = Image.open(first).convert("RGB"), Image.open(png).convert("RGB")
        if a.size != b.size or a.tobytes() != b.tobytes():
            raise RuntimeError("pixels differ after conversion: " + first)
    except ImportError:
        pass
    data = open(png, "rb").read(); cs = m.chunks(data)
    k, packet = m.read_xmp(cs)
    packet, facts = m.prepare(packet if packet else m.EMPTY_PACKET, stem, media_created=media_created, set_id=SET)
    prepared = m.with_xmp(data, cs, k, packet)
    row("delivering", first, media_created_date=media_created)
    res, err = m.deliver(PAND, STORE, prepared)
    if err:
        row("refused", first, media_created_date=media_created, reason=err); counts["refused"] = counts.get("refused", 0) + 1
        print("refused", first, err, flush=True); continue
    export(first)
    row("stored", first, media_created_date=media_created, pan_id=res.get("id", ""), pan_media_path=res.get("media_path", ""),
        reason="JPEG under a .png name; decoded and stored as PNG")
    counts["stored"] = counts.get("stored", 0) + 1
    for p in rest:
        export(p)
        row("duplicate", p, reason="byte-identical to %s" % first, pan_id=res.get("id", ""))
        counts["duplicate"] = counts.get("duplicate", 0) + 1
out.close(); print("done:", counts)
