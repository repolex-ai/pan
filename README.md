# Pan

> **Pan is a standalone media store that speaks git-lex: it stores media, describes it
> with a graph, and searches it by both graph pattern and vector similarity.**

That sentence is the ruler. Anything that is not *store media / describe it / search it*
is out of scope and belongs in a different tool.

Spec: `repolex-ai/subtexture/docs/pan/2026_07_15_PAN_SPEC.md`. Pan is the clean
re-extraction of the one job Pool was meant to do; the proven RDF+vector fusion engine
is lifted from Pool intact, the accretions (queue, router, security gate, complex
identifiers) are deliberately left behind.

## Quick start (Mode 1 — standalone)

```sh
pan serve --root ~/pan-store          # starts the API server
open http://127.0.0.1:7401/swagger-ui # the Swagger IS the interface spec
```

Configuration is one optional file, `<root>/pan.yml`:

```yaml
storage_id: my-store        # identity for this store (default: "default")
storage_root: /Volumes/big  # optional — relocates MEDIA only; graph + index stay local
index_id: my-embedder-2048  # default vector index name
detectors:                  # all optional; Pan ships zero models
  embed: http://127.0.0.1:1215/embed
```

**Two query modes, both first-class:**
1. **Graph-only** — pure SPARQL. Works with no detector configured and no vectors
   present. A fresh install is a complete product.
2. **Graph + vector** — SPARQL prefilter fused with cosine kNN, when vectors exist.

## What Pan is NOT

No processing queue. No multi-soul router (that is Syrinx). No security model beyond
identity validation. No bundled model weights — detectors are configured external
endpoints. No render orchestration.

## Layout

```
<root>/
  pan.yml            optional config
  oxigraph/          RDF graph store — always local, never relocated
  hnsw/              vector index — always local, never relocated
  storage/           media (blobs + raw vector sidecars) — the ONE overridable root
    blob/image/YYYY/MM/DD/<panId>.png
    vectors/<index>/...npy
```
