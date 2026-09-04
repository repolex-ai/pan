# Pan

> **Pan is a media store that speaks git-lex: it stores media, describes it
> with a graph, and searches it by both graph pattern and vector similarity.**

That sentence is the ruler. Anything that is not *store media / describe it /
search it* is out of scope and belongs in a different tool.

Two binaries, one crate:

- **`pand`** — the daemon. One per machine. It owns every Pan store on the
  machine: it is the only thing that touches a store's files or writes its
  graph, it makes every model call (one funnel, with a concurrency limit per
  model), and it keeps filesystem and graph in step in small atomic steps.
  Zero arguments; everything is in `~/.config/pan/config.yml`.
- **`pan`** — the command line. A thin client of pand. Every answer it prints
  came from the graph; it never reads a store directly.

git-lex and Syrinx are readers of the stores; Horae (rendering) delivers into
pand exactly as it delivered into Pool.

## Quick start

```sh
pand                                   # foreground; ctrl-c stops it
pan store  ~/Pictures/wolf.png         # → <pan/Image/k7m2p9x4>
pan state  '<pan/Image/k7m2p9x4>'      # thumbnail, embed, caption, pose: done / pending / off
pan info   '<pan/Image/k7m2p9x4>'      # every fact the graph holds about it
pan query  'SELECT ?m ?t WHERE { ?s pan:captionItem ?c . ?c pan:model ?m ; pan:text ?t }'
pan stores                             # the stores this machine's pand manages
open http://127.0.0.1:7401/swagger-ui  # the Swagger IS the interface spec
```

`pan store [<user-id>] <file>` — `<user-id>` names a store (a soul's genesis
SHA, or a bare store's id); absent = pand's default.

## Configuration — `~/.config/pan/config.yml`

A missing file means one store at `~/.pan` and no model stages.

```yaml
stores:
  - /Users/rob/repos/7R1PL3F0RC3/lUX     # a soul repo → store at <repo>/.pan, id = genesis SHA
  - ~/.pan                               # a bare store → id from its own pan.yml (storage_id)
default: /Users/rob/repos/7R1PL3F0RC3/lUX
port: 7401
interval_secs: 5                         # pause between stage passes when nothing is pending
batch: 8                                 # images per stage per store per pass
models:                                  # every stage optional; pand ships zero models
  embed:
    url: http://127.0.0.1:1215/see_embed
    model: qwen3-vl-embedding-2b-8bit    # recorded as pan:model on every Embedding
    caption_model: qwen3.5-9b-mlx-8bit   # /see_embed also captions; recorded under this name
    concurrency: 1
  pose:
    url: http://127.0.0.1:1215/see_pose
    model: rtmw-x-l
    enabled: false                       # test mode: declared, never called; flip on later
```

`enabled: false` is the test-mode switch: the stage stays declared, pand never
calls it, ingest still lands, `pan state` reports it `off`. Because the graph is
the queue, turning it back on picks up every image missing that model's record.

A soul repo's `.pan/` must be gitignored (media is never git history); pand
warns at start if it is not.

## How an image gets in

`POST /media` (Horae's delivery body, or `pan store`), in this order:

1. bytes written to `media/image/YYYY/MM/DD/<id>.png`
2. Pan's XMP packet written into the PNG (identity, thumbnail, enrichment references)
3. thumbnail made (512px JPEG) beside it
4. graph node committed — ONE transaction. The image exists only after this.

Then the **stage ladder**: every pass, per store, per configured stage, pand
asks the graph *which images have no record from this model*, takes a bounded
batch, calls the model, and writes the data file + graph + XMP for each one.
The graph is the queue. A failed call leaves the image pending (with an
in-memory hold so it is not retried every pass); success is only ever the
record in the graph. `pan state` reads that record.

## Layout of one store

```
<root>/                       soul repo: <repo>/.pan   bare: the configured dir
  pan.yml                     optional (storage_id, storage_root, prefixes)
  pan.ttl                     reference copy of the ontology
  oxigraph/                   the graph — always local, never relocated
  hnsw/<model>/               vector index per embedding model — always local
  storage/                    the ONE relocatable root (pan.yml storage_root:)
    media/image/YYYY/MM/DD/<id>.png
    thumbnail/YYYY/MM/DD/<id>.jpg
    vectors/<model>/<id>.npy
    caption/YYYY/MM/DD/<id>.<model>.xml
    pose/YYYY/MM/DD/<id>.xml  (+ <id>.<model>.png skeleton overlay)
    sam3/YYYY/MM/DD/<id>.xml
```

Every path above is declared in the graph and in the image's own XMP; nothing
is found by convention.

## What Pan is NOT

No processing queue table. No multi-soul router (that is Syrinx). No security
model beyond identity validation. No bundled model weights.
