#!/usr/bin/env python3
"""Remove one pan: field from every image in a store, through pand's unset
route, one image at a time (pand is the only writer; each call rewrites the
image's XMP). Written to take pan:enrichmentCompleteDate off the 204,922
images it was wrongly written on (goodlux, 2026-10-03). Safe to rerun: it
asks the graph which images still carry the field and stops when none do.

  python3 migration/unset_everywhere.py --store 700c5b --field enrichmentCompleteDate
"""
import argparse, json, sys, time, urllib.request, urllib.error

PAN = "https://repolex.ai/ontology/pan/"

def post(url, body):
    req = urllib.request.Request(url, data=json.dumps(body).encode(), method="POST",
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=300) as r:
        return json.loads(r.read().decode())

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--store", required=True)
    ap.add_argument("--field", required=True)
    ap.add_argument("--pand", default="http://127.0.0.1:7401")
    ap.add_argument("--batch", type=int, default=2000)
    a = ap.parse_args()
    done, failed, t0 = 0, 0, time.time()
    while True:
        q = "SELECT ?s WHERE { ?s <%s%s> ?v } LIMIT %d" % (PAN, a.field, a.batch)
        ids = [b["s"]["value"] for b in post(a.pand + "/query", {"store": a.store, "query": q})["results"]["bindings"]]
        if not ids:
            break
        for iri in ids:
            bare = iri.rsplit("/", 1)[-1]
            try:
                post("%s/media/%s/unset" % (a.pand, bare), [a.field])
                done += 1
            except urllib.error.HTTPError as e:
                failed += 1
                print("failed", iri, e.code, e.read().decode("utf-8", "replace")[:200], flush=True)
                if failed > 50:
                    print("too many failures, stopping", flush=True); sys.exit(1)
            except urllib.error.URLError as e:
                print("pand unreachable: %s; stopping" % e.reason, flush=True); sys.exit(1)
        print("%s: removed %d (%.0f/min)" % (a.store, done, done / max(time.time() - t0, 1) * 60), flush=True)
    print("done: store %s field %s removed from %d images, %d failures" % (a.store, a.field, done, failed), flush=True)

if __name__ == "__main__":
    main()
