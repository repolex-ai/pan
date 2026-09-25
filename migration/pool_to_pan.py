#!/usr/bin/env python3
"""Pool → Pan, one file at a time. A throwaway for the 2026 Pool migration
(repolex-ai/pan #49, #66, #67; ruled by goodlux 2026-09-22 and 2026-09-24).

For each Pool PNG, in path order:
  1. archive its XMP packet whole, as a sidecar under --archive/xmp/…;
  2. prepare a copy: the root Description gets rdf:about="" so its facts are
     about the image; the pool: fields go (the content id dies); the region
     and pose sub-Descriptions go (their subjects are the same fixed names in
     every file, so in one graph they would all collapse into one node — the
     sidecar keeps them); a pan Description is added carrying
     pan:mediaCreatedDate from the file name (UTC → system time) and
     pan:relatedToId for the Moment (copia:momentId, else copia:cid) and the
     set (copia:inSetId as <pan/ImageSet/…>);
  3. deliver it to pand exactly as Horae does, POST /stores/<id>/media;
  4. on success, move the original to --exported under the same relative
     path; on any failure, leave it where it is;
  5. append one row to --archive/mapping.csv either way.

Re-running skips every file the table already says was stored. A file whose
last row says "delivering" (the run died between sending and recording) is
looked up in pand by its media date and Moment id before anything is sent
again, so an interruption never stores a file twice. Nothing here touches a
file pand refused; the table says why, and a person decides.
"""
import argparse, csv, datetime as dt, json, os, re, signal, struct, sys, urllib.request, urllib.error, zlib

PAN_NS = "https://repolex.ai/ontology/pan/"
XMP_KEY = b"XML:com.adobe.xmp"
SUB_DROP = re.compile(r'rdf:about="(Sam3Region|PoseDetection):')
COLUMNS = ["when", "status", "source_path", "stem", "old_cid", "old_moment_id", "old_in_set_id",
           "media_created_date", "store", "pan_id", "pan_media_path", "sidecar", "reason"]


# ── PNG chunks ───────────────────────────────────────────────────────────────
def chunks(b):
    if b[:8] != b"\x89PNG\r\n\x1a\n":
        raise ValueError("not a PNG")
    i, out = 8, []
    while i < len(b):
        n = struct.unpack(">I", b[i:i + 4])[0]
        t = b[i + 4:i + 8]
        out.append((t, b[i + 8:i + 8 + n]))
        i += 12 + n
        if t == b"IEND":
            break
    return out

def chunk_bytes(t, data):
    return struct.pack(">I", len(data)) + t + data + struct.pack(">I", zlib.crc32(t + data) & 0xffffffff)

def read_xmp(cs):
    for k, (t, d) in enumerate(cs):
        if t == b"iTXt" and d.startswith(XMP_KEY + b"\x00"):
            return k, d.split(b"\x00", 5)[-1].decode("utf-8")
    return None, None

def with_xmp(b, cs, k, packet):
    data = XMP_KEY + b"\x00\x00\x00\x00\x00" + packet.encode("utf-8")
    out = b[:8]
    for j, (t, d) in enumerate(cs):
        out += chunk_bytes(t, data if j == k else d)
    return out


# ── the packet ───────────────────────────────────────────────────────────────
def field(packet, local):
    """One copia field's text, whichever spelling the era used."""
    m = re.search(r"<copia:%s(?:\s[^>]*)?>([^<]*)</copia:%s>" % (local, local), packet)
    return m.group(1).strip() if m else None

def xml_escape(s):
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")

def prepare(packet, stem, keep_subs=False):
    """Return (new_packet, facts) or raise ValueError."""
    m = re.match(r"(\d{8})-(\d{6})-[0-9a-f]{8}$", stem)
    if not m:
        raise ValueError("file name is not YYYYMMDD-HHMMSS-cid8")
    utc = dt.datetime.strptime(m.group(1) + m.group(2), "%Y%m%d%H%M%S").replace(tzinfo=dt.timezone.utc)
    media_created = utc.astimezone().isoformat(timespec="seconds")

    old_cid = field(packet, "cid") or field(packet, "Cid")
    pm = re.search(r"<pool:cid>([^<]*)</pool:cid>", packet)
    if pm:
        old_cid = pm.group(1).strip()
    moment = field(packet, "momentId") or field(packet, "Cid") or field(packet, "cid")
    # Before August the Moment id was the content id itself.
    if not old_cid and moment and moment.startswith("sha256:"):
        old_cid = moment
    in_set = field(packet, "inSetId")
    if in_set and not re.fullmatch(r"[A-Za-z0-9_-]{1,200}", in_set):
        raise ValueError("copia:inSetId is not an id pand can use: %r" % in_set)
    if moment and not re.fullmatch(r"[A-Za-z0-9:_-]{1,200}", moment):
        raise ValueError("moment id is not usable: %r" % moment)

    # The root Description: the first one. Give it rdf:about="" if it has none.
    first = re.search(r"<rdf:Description\b([^>]*)>", packet)
    if not first:
        raise ValueError("no rdf:Description in the packet")
    attrs = first.group(1)
    if "rdf:about=" not in attrs:
        packet = packet[:first.start()] + '<rdf:Description rdf:about=""' + attrs + ">" + packet[first.end():]
    # The pool: fields die.
    packet = re.sub(r"\s*<pool:\w+(?:\s[^>]*)?>[^<]*</pool:\w+>", "", packet)
    packet = re.sub(r"\s*<pool:\w+(?:\s[^>]*)?/>", "", packet)
    # Region and pose sub-Descriptions go (kept in the sidecar).
    if not keep_subs:
        packet = re.sub(r"\s*<rdf:Description\b[^>]*rdf:about=\"(?:Sam3Region|PoseDetection):[^\"]*\"[^>]*>.*?</rdf:Description>",
                        "", packet, flags=re.S)
    # Pan's fields, in a Description of their own about the image.
    lines = ['<pan:mediaCreatedDate>%s</pan:mediaCreatedDate>' % media_created]
    if moment:
        lines.append('<pan:relatedToId>%s</pan:relatedToId>' % xml_escape("<copia/Moment/%s>" % moment))
    if in_set:
        lines.append('<pan:relatedToId>%s</pan:relatedToId>' % xml_escape("<pan/ImageSet/%s>" % in_set))
    pan_desc = '    <rdf:Description rdf:about="" xmlns:pan="%s">\n      %s\n    </rdf:Description>\n' % (PAN_NS, "\n      ".join(lines))
    end = packet.rfind("</rdf:RDF>")
    if end < 0:
        raise ValueError("no </rdf:RDF> in the packet")
    packet = packet[:end] + pan_desc + packet[end:]
    return packet, {"old_cid": old_cid or "", "old_moment_id": moment or "", "old_in_set_id": in_set or "",
                    "media_created_date": media_created}


# ── delivery ─────────────────────────────────────────────────────────────────
def already_stored(pand, store, facts):
    """The pan id of an image pand holds with this media date and Moment id,
    or None. Used only for a file whose delivery was cut off unrecorded."""
    moment = facts["old_moment_id"]
    q = ('SELECT ?s WHERE { GRAPH ?g { ?s <%smediaCreatedDate> "%s" . %s } } LIMIT 2'
         % (PAN_NS, facts["media_created_date"],
            '?s <https://repolex.ai/ontology/copia/momentId> "%s" .' % moment if moment and not moment.startswith("sha256:") else
            ('?s <%srelatedToId> <https://repolex.ai/copia/Moment/%s> .' % (PAN_NS, moment) if moment else "")))
    req = urllib.request.Request("%s/query" % pand, data=json.dumps({"store": store, "query": q}).encode(),
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=60) as r:
        hits = json.loads(r.read().decode())["results"]["bindings"]
    if len(hits) == 1:
        return hits[0]["s"]["value"]
    return None

def deliver(pand, store, png):
    req = urllib.request.Request("%s/stores/%s/media" % (pand, store), data=png, method="POST",
                                 headers={"Content-Type": "image/png"})
    try:
        with urllib.request.urlopen(req, timeout=120) as r:
            return json.loads(r.read().decode()), None
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", "replace")
        try:
            body = json.loads(body).get("error", body)
        except Exception:
            pass
        return None, "pand %d: %s" % (e.code, body)
    except urllib.error.URLError as e:
        return None, "pand unreachable: %s" % e.reason


# ── the run ──────────────────────────────────────────────────────────────────
def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--source", required=True, help="the Pool image tree, e.g. …/pool/blob/image/2026")
    ap.add_argument("--exported", required=True, help="where stored files go, same relative path")
    ap.add_argument("--archive", required=True, help="sidecars and mapping.csv")
    ap.add_argument("--pand", default="http://127.0.0.1:7401")
    ap.add_argument("--store", required=True, help="store id, e.g. 700c5b")
    ap.add_argument("--limit", type=int, default=0, help="stop after this many files considered")
    ap.add_argument("--dry-run", action="store_true", help="prepare only; write the prepared PNGs under --archive/dry-run")
    ap.add_argument("--keep-sub-descriptions", action="store_true")
    a = ap.parse_args()

    os.makedirs(a.archive, exist_ok=True)
    mapping = os.path.join(a.archive, "mapping.csv")
    done, last = set(), {}
    if os.path.exists(mapping):
        with open(mapping, newline="") as f:
            for r in csv.DictReader(f):
                last[r["source_path"]] = r["status"]
                if r["status"] == "stored":
                    done.add(r["source_path"])
    uncertain = {p for p, st in last.items() if st == "delivering"}
    stop = {"now": False}
    def on_signal(sig, frame):
        print("stopping after the current file (signal %d)" % sig, flush=True)
        stop["now"] = True
    signal.signal(signal.SIGINT, on_signal)
    signal.signal(signal.SIGTERM, on_signal)
    files = []
    for dp, dn, fn in os.walk(a.source):
        dn.sort()
        for n in sorted(fn):
            if n.endswith(".png"):
                files.append(os.path.join(dp, n))
    files.sort()
    print("files: %d, already stored: %d, uncertain: %d" % (len(files), len(done), len(uncertain)), flush=True)

    new_table = not os.path.exists(mapping)
    out = open(mapping, "a", newline="")
    w = csv.DictWriter(out, fieldnames=COLUMNS)
    if new_table:
        w.writeheader()
    counts = {}
    considered = 0

    def row(status, path, stem, facts=None, **kw):
        r = {c: "" for c in COLUMNS}
        r.update(when=dt.datetime.now().astimezone().isoformat(timespec="seconds"), status=status,
                 source_path=path, stem=stem, store=a.store)
        if facts:
            r.update(facts)
        r.update(kw)
        w.writerow(r); out.flush()
        counts[status] = counts.get(status, 0) + 1

    for path in files:
        if path in done:
            continue
        if stop["now"] or (a.limit and considered >= a.limit):
            break
        considered += 1
        rel = os.path.relpath(path, a.source)
        stem = os.path.splitext(os.path.basename(path))[0]
        try:
            with open(path, "rb") as f:
                b = f.read()
        except OSError as e:
            row("unreadable", path, stem, reason=str(e)); continue
        try:
            cs = chunks(b)
        except ValueError as e:
            row("skipped", path, stem, reason=str(e)); continue
        k, packet = read_xmp(cs)
        if packet is None:
            row("skipped", path, stem, reason="no XMP in the file"); continue
        try:
            new_packet, facts = prepare(packet, stem, a.keep_sub_descriptions)
        except ValueError as e:
            row("skipped", path, stem, reason="prepare: %s" % e); continue
        prepared = with_xmp(b, cs, k, new_packet)
        if a.dry_run:
            dst = os.path.join(a.archive, "dry-run", rel)
            os.makedirs(os.path.dirname(dst), exist_ok=True)
            with open(dst, "wb") as f:
                f.write(prepared)
            row("dry-run", path, stem, facts, reason=dst); continue
        # The sidecar first: the packet as it was, before anything else happens.
        side = os.path.join(a.archive, "xmp", os.path.splitext(rel)[0] + ".xmp")
        os.makedirs(os.path.dirname(side), exist_ok=True)
        with open(side, "w", encoding="utf-8") as f:
            f.write(packet)
        res, err = None, None
        if path in uncertain:
            try:
                found = already_stored(a.pand, a.store, facts)
            except Exception as e:
                row("refused", path, stem, facts, sidecar=side, reason="uncertain and pand could not be asked: %s" % e); continue
            if found:
                res = {"id": "<pan/Image/%s>" % found.rsplit("/", 1)[-1], "media_path": ""}
                print("uncertain file was already in pand: %s -> %s" % (stem, found), flush=True)
        if res is None:
            row("delivering", path, stem, facts, sidecar=side)
            res, err = deliver(a.pand, a.store, prepared)
        if err:
            row("refused", path, stem, facts, sidecar=side, reason=err)
            if err.startswith("pand unreachable"):
                print("stopping: " + err, flush=True); break
            continue
        dst = os.path.join(a.exported, rel)
        os.makedirs(os.path.dirname(dst), exist_ok=True)
        try:
            os.rename(path, dst)
        except OSError as e:
            row("stored-not-moved", path, stem, facts, sidecar=side, pan_id=res.get("id", ""),
                pan_media_path=res.get("media_path", ""), reason="move: %s" % e); continue
        row("stored", path, stem, facts, sidecar=side, pan_id=res.get("id", ""), pan_media_path=res.get("media_path", ""))
        if counts.get("stored", 0) % 500 == 0:
            print("stored %d" % counts["stored"], flush=True)
    out.close()
    print("done:", json.dumps(counts), flush=True)

if __name__ == "__main__":
    main()
