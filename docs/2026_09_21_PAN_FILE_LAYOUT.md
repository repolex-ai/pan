# Pan file layout

Where Pan puts every file, and the rules the names follow. Decided by
goodlux on 2026-09-16, and revised on 2026-09-18 and 2026-09-21: model output
moved from `data/` to `enrichment/`, every enrichment file took one naming
rule, records became N-Quads, and the set folder became `ImageSet/`. This is
what `pand` writes now; nothing older is migrated.

## The tree

Two roots. The **store root** (`<repo>/.pan`, or the bare store directory)
is committed with the soul, all but its `_ignore/` pocket; the **media root**
holds the pictures and the records, off the system drive when a volume is
configured.

```
<store root>/                       <repo>/.pan, or the bare store directory
├── pan.yml                         config (optional)
├── ImageSet/                       one file per curated set, the folder named for the class; committed
│   └── abcd2345.nq
└── _ignore/                        machine-local, never committed
    ├── oxigraph/                   the graph
    ├── hnsw/<model>/               the vector index per embedding model
    └── media/                      the media root, when no volume is configured

<media root>/                       one per store; <volume>/_pan/<6-char id>/pan, or <store>/_ignore/media
└── image/                          one folder per media kind (image, video, audio)
    ├── img/                        PIXELS: the pictures and their renditions
    │   ├── original/YYYY/MM/DD/
    │   │   └── 20260916-101500-abcd1234.jpg
    │   ├── source/YYYY/MM/DD/
    │   │   └── 20260916-101500-abcd1234.png
    │   ├── jpg/YYYY/MM/DD/
    │   │   └── 20260916-101500-abcd1234_512.jpg
    │   └── upscale/YYYY/MM/DD/
    │       └── 20260916-101500-abcd1234_4096.png
    └── enrichment/                 MODEL OUTPUT: records about the pictures
        ├── caption/YYYY/MM/DD/
        │   └── 20260916-101500-abcd1234.caption.qwen3-8-27b.nq
        ├── segment/YYYY/MM/DD/
        │   ├── 20260916-101500-abcd1234.segment.sam3.nq
        │   └── 20260916-101500-abcd1234.segment.sam3.json
        ├── pose/YYYY/MM/DD/
        │   ├── 20260916-101500-abcd1234.pose.rtmw-x-l.nq
        │   └── 20260916-101500-abcd1234.pose.rtmw-x-l.png
        ├── depth/YYYY/MM/DD/
        │   ├── 20260916-101500-abcd1234.depth.depth-anything-v2-base.nq
        │   ├── 20260916-101500-abcd1234.depth.depth-anything-v2-base.png
        │   └── 20260916-101500-abcd1234.depth.depth-anything-v2-base.json
        └── embed/YYYY/MM/DD/
            ├── 20260916-101500-abcd1234.embed.qwen3-vl-embedding-2b.nq
            ├── 20260916-101500-abcd1234.embed.qwen3-vl-embedding-2b.npy
            └── 20260916-101500-abcd1234.embed.qwen3-vl-embedding-2b.json
```

`img/` holds pictures. `enrichment/` holds what models said about them. One glob
finds all of either, and neither side needs to know the other's folder names.

## Each folder

- **`ImageSet/`** — one N-Quads file per set a person curates, named by
  the set's id, in a folder named for the class. A set carries exactly its
  id, its description and its created date; it keeps no member list.
  Membership is `pan:relatedToId` on the image, written into the image's
  XMP and the graph. On every open the store reads these files and rewrites
  the set nodes in the graph from them, so the graph is rebuilt from files
  alone. A file that says anything else, or describes a second node, stops
  the store from opening.
- **`img/original/`** — the file exactly as it arrived, when it was not a
  PNG (JPEG, WebP, GIF, TIFF). Kept for the record. Nothing reads it again.
  A PNG arrival has no entry here.
- **`img/source/`** — the image Pan works from. Always PNG. Pan's XMP is
  written inside it, and every stage reads this file and no other. When the
  arrival was not PNG, this is the decoded pixels written as PNG; the pixels
  are the same as the decoder saw, verified by the conversion's own test.
  The image carries `pan:sourceFile`, the path of the file it was made from:
  the `img/original/` file when the arrival was converted, its own path when
  the arrival was already PNG. It is always present and pand writes it; it
  cannot be set by hand.
- **`img/jpg/`** — derived JPEG renditions. Today there is one, the 512 px
  thumbnail. Other sizes go in the same folder with their own suffix.
- **`img/upscale/`** — upscaled renditions, named by their long edge like
  every other size. An upscale stays PNG, like the source, so nothing is
  lost between the model that made it and the reader. Reserved (goodlux,
  2026-09-16); no stage writes here yet.
- **`enrichment/caption/`** — one record per caption run: the reference,
  the Caption record, and the model's whole answer verbatim.
- **`enrichment/pose/`** — one record per pose run, one Pose record per
  person, plus the model's skeleton overlay as a PNG beside it.
- **`enrichment/segment/`** — one record per segmentation run, one Region
  record per thing outlined, and the server's whole answer as `.json`.
- **`enrichment/depth/`** — one record per depth run with one Depth record,
  the depth map as an 8-bit grayscale PNG beside it (bright is near: 255 is
  the nearest point, 0 the farthest, normalized per image from the record's
  min and max), and everything else the node said as `.json`.
- **`enrichment/embed/`** — one record per image per embedding model holding
  the vectorData reference and its Embedding record (id, model, dim,
  precision, provider, producedDate, vectorPath), the `.npy` vector it names
  beside it, and the server's full answer as `.json`. The searchable copy
  lives in the store's index; these files are the rebuild source.

Where a stage saves the server's own answer as `.json`, the reference in the
graph names it with `pan:modelReplyPath`.

## Records are N-Quads

A record file is N-Quads, `.nq`: one statement per line, the same statements
the graph holds, serialized. Every line names Pan's one named graph in its
fourth column, `<https://repolex.ai/pan/NamedGraph/pan>`. The graph is the
same in every store. A query with no `GRAPH` clause reads it.

## Naming rules

**The stem.** `YYYYMMDD-HHMMSS-<id>`: the date and time the image was stored,
in system local time, and its eight-character Pan id. The id is the same one
the graph uses, `<pan/Image/abcd1234>`. Readers never parse the stem; the
graph's `pan:mediaPath` is the path.

**Sizes are suffixes, never folders.** A derived rendition carries its long
edge in the file name: `_512`, `_1024`, `_2048`. There is no `thumbnail/`, no
`large/`, no `preview/`. A folder names a format or a kind of rendition
(`jpg/`, `upscale/`); the suffix names the size. When a size changes,
nothing is renamed but the file.

**Square crops, reserved.** If a grid ever needs a square crop, it is
`_<edge>_sq`, for example `…_512_sq.jpg`. Not built.

**Enrichment files.** Every file a stage writes is named
`<source file name>.<stage>.<model>.<ext>`: the picture it is about, the
stage that made it, the model that ran, and the kind of file. The stage is
one of `caption`, `segment`, `pose`, `depth`, `embed`. The model is the
`model:` name from `config.yml`, lowercase with dashes. The extension is `nq`
for the record, `json` for the server's answer, `png` for a map or an
overlay, `npy` for a vector.

**Date shards everywhere.** Every per-image file sits under `YYYY/MM/DD/` of
the image's stored date, so no folder ever holds more than one day's images.
Vectors are dated like everything else.

## What the graph knows

Every path in the graph (`pan:mediaPath`, `pan:path`, `pan:vectorPath`) is
relative to the media root, and the media root is a fact on the store's own
node (`pan:mediaRoot`). A reader resolves a path by joining the two; it never
guesses from a naming convention.
