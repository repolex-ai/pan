# Pan file layout

Where Pan puts every file, and the rules the names follow. Ruled by goodlux,
2026-09-16. This is what `pand` writes from that day on; nothing older is
migrated.

## The tree

Two roots. The **store root** (`<repo>/.pan`, or the bare store directory)
is committed with the soul, all but its `_ignore/` pocket; the **media root**
holds the pictures and the records, off the system drive when a volume is
configured.

```
<store root>/                       <repo>/.pan, or the bare store directory
├── pan.yml                         config (optional)
├── imagesets/                      one file per curated set; committed
│   └── abcd2345.xml
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
    └── data/                       MODEL OUTPUT: records about the pictures
        ├── caption/YYYY/MM/DD/
        │   └── abcd1234.qwen-qwen3.8-27b.xml
        ├── pose/YYYY/MM/DD/
        │   ├── abcd1234.xml
        │   └── abcd1234.rtmw-x-l.png
        ├── sam3/YYYY/MM/DD/
        │   └── abcd1234.xml
        ├── depth/YYYY/MM/DD/
        │   ├── abcd1234.xml
        │   ├── abcd1234.depth-anything-Depth-Anything-V2-Base-hf.png
        │   └── abcd1234.depth-anything-Depth-Anything-V2-Base-hf.json
        └── vectors/
            └── qwen3-vl-embedding-2b/
                ├── abcd1234.npy
                └── abcd1234.json
```

`img/` holds pictures. `data/` holds what models said about them. One glob
finds all of either, and neither side needs to know the other's folder names.

## Each folder

- **`imagesets/`** — one XMP-style file per set a person curates, named by
  the set's id. A set carries exactly its id, its description and its
  created date; it keeps no member list. Membership is `pan:relatedToId`
  on the image, written into the image's XMP and the graph. On every open
  the store reads these files and rewrites the set nodes in the graph from
  them, so the graph is rebuilt from files alone (issue #4; pan.ttl 0.4.2).
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
- **`data/caption/`** — one XML file per caption run: the reference, the
  Caption record, and the model's whole answer verbatim.
- **`data/pose/`** — one XML file per pose run, one Pose record per person,
  plus the model's skeleton overlay as a PNG beside it.
- **`data/sam3/`** — one XML file per segmentation run, one Region record per
  thing outlined.
- **`data/depth/`** — one XML file per depth run with one Depth record, the
  depth map as an 8-bit grayscale PNG beside it (bright is near: 255 is the
  nearest point, 0 the farthest, normalized per image from the record's
  min and max), and everything else the node said as `.json` (issue #24).
- **`data/vectors/<model>/`** — one `.npy` vector per image per embedding
  model, with the server's full answer beside it as `.json`. The searchable
  copy lives in the store's index; these files are the rebuild source.

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

**Model ids in file names.** A model id can contain a slash
(`qwen/qwen3.8-27b`). In a file name every slash becomes a dash
(`abcd1234.qwen-qwen3.8-27b.xml`, `vectors/qwen-qwen3.8-27b/`), because a
slash in a path is a folder. The graph's `pan:model` keeps the real id.

**Date shards everywhere.** Every per-image file sits under `YYYY/MM/DD/` of
the image's stored date, so no folder ever holds more than one day's images.
Vectors are the exception: they are keyed by model, and the index rebuild
reads them all at once.

## What the graph knows

Every path in the graph (`pan:mediaPath`, `pan:path`, `pan:vectorPath`) is
relative to the media root, and the media root is a fact on the store's own
node (`pan:mediaRoot`). A reader resolves a path by joining the two; it never
guesses from a naming convention.
