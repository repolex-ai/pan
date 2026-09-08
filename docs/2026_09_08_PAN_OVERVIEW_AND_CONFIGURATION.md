# Pan — overview and configuration

Pan is a media store. It keeps media files, writes what it knows about each
file into the file's own XMP metadata, describes every file in a graph, and
searches by graph pattern and by vector similarity. It can fill in metadata by
calling models (a caption, an embedding, a pose, regions), but it does not
decide what those models should be asked. That comes from configuration and
from the ontologies installed for a store.

Two programs, one crate:

- `pand` — the daemon. One per machine. The only thing that writes to a store.
- `pan` — the command line. A client of `pand`. Never touches a store directly.

Things marked **planned** in this document are designed but not built yet.
Everything else is running today.

## Quick install

```sh
git clone git@github.com:repolex-ai/pan.git && cd pan
cargo install --path . --locked        # installs pand and pan
```

Then `pand start` in a terminal. It runs there and stops with `pand stop`.

## Pan as a solo store

A store is one folder. Media goes in with `pan store <file>` and is copied
into the store's media root. Everything Pan derives sits beside it, by media
kind, then by what the file is.

```
<media root>/                          the store's media, one folder per media kind
└── image/
    ├── source/YYYY/MM/DD/             the media files, bytes as delivered, XMP written in
    │   └── 20260907-043526-v5ha2dfd.png
    ├── thumbnail/YYYY/MM/DD/          512 px JPEG per file
    ├── caption/YYYY/MM/DD/            one XML record per model run, beside the model's raw answer
    ├── pose/YYYY/MM/DD/               keypoints per person, plus an overlay image
    ├── sam3/YYYY/MM/DD/               regions per prompt (bbox, polygon, score), plus the raw answer
    └── vectors/<model>/               one .npy per image, plus the server's answer as .json

<store root>/                          the store itself
├── pan.yml                            the store's own settings (today: its id)
└── _ignore/
    ├── oxigraph/                      the graph: every fact about every file
    ├── hnsw/<model>/                  the vector index, one per embedding model
    └── pan.ttl                        reference copy of the Pan ontology
```

A bare store puts its media root inside `_ignore/media/`. When a media volume
is configured, the media root is `<volume>/_pan/<first 6 chars of the store
id>/pan/` and the folder above is what you find there.

The file name of a stored image is `<local date>-<local time>-<id>.<ext>`.
The id is the last eight characters and is the same id the graph uses:
`<pan/Image/v5ha2dfd>`.

The image's XMP carries everything Pan knows about it under the `pan`
namespace: id, creation time, path, type, size, the short and long
descriptions, the scene objects, the scene fields, the thumbnail, and one
reference per model run. What a producer wrote into the file before it
arrived is kept untouched.

## Pan as an indexer of existing media — planned

A store can point at media that already exists, wherever it is, and index it
in place. Nothing is copied or moved. Pan writes XMP into the files it can
(JPEG, PNG, DNG) and a standard `.xmp` sidecar next to the ones it cannot
(camera RAW).

```
/Volumes/photos-2019/                  your folder, untouched
├── IMG_0001.jpg                       gets XMP written in
├── IMG_0002.cr2
├── IMG_0002.xmp                       sidecar for the RAW
└── .pan/                              the store, travelling with the photos
    ├── pan.yml                        media root: ".", stages, prompts, ontologies
    └── _ignore/
        ├── oxigraph/
        ├── hnsw/
        └── derived/                   thumbnails and model records, out of your folders
```

The store lives next to the photos, so the graph and the thumbnails go where
the drive goes. The daemon serves the store while the drive is mounted.

## Pan as part of the Subtexture stack

One `pand` per machine serves every store on it. In Subtexture each agent has
its own store inside its soul repository, and one daemon serves all of them
with one set of model endpoints. Rendering (Horae) delivers finished images
straight into a store with the producer's metadata already in the file.
git-lex reads each store's graph directly, so a soul's own knowledge and its
media answer one query. Syrinx does the same across every store on the
machine.

**Planned:** stores are served in the order they are listed. A stage moves on
to the next store only when the one above it has nothing pending, so the
agents' work comes first and other collections are filled in when the models
are idle.

## Configuration

### File locations

| File | Scope | What it is for |
|---|---|---|
| `~/.config/pan/config.yml` | this machine | the daemon: port, the stores it serves, model endpoints and their auth, concurrency ceilings, backfill floor |
| `~/.config/pan/prompts/<name>` | this machine | prompt text, plain text, one file per stage; the config names the file |
| `~/.config/pan/logs/calls/YYYY-MM-DD.jsonl` — planned | this machine | one line per model call: store, image, stage, model, status, latency, sizes; pruned after `log_keep_days` |
| `<store>/pan.yml` | one store | the store's id; **planned:** its media root, which stages run, which prompt file each uses, its backfill floor, which ontologies apply |
| `<store>/_ignore/` | one store | the graph, the vector index, the ontology copy; never edited by hand |
| `~/.pan/logs/pand.log` | this machine | the daemon's log, also printed in the terminal that started it |

### The daemon file, `~/.config/pan/config.yml`

```yaml
stores:                                  # every store this daemon serves, in priority order
  - /Users/me/repos/squad/agent-a        # a soul repo: store at <repo>/.pan, id = the repo's genesis SHA
  - ~/.pan                               # a bare store: id from its own pan.yml
default: ~/.pan                          # where `pan store <file>` lands when no store is named
media_volume: /Volumes/p02/_pan          # media root = <volume>/<6-char id>/pan; omit to keep media in the store
port: 7401
batch: 4                                 # images per stage per store per pass
interval_secs: 5                         # pause between passes when nothing is pending
backfill_since: "2026-09-07T04:15:45-07:00"   # images created before this are not sent to models
models:
  caption:
    url: http://127.0.0.1:1215/percept/vlm
    model: qwen/qwen3.8-27b
    prompt: caption.md                   # a file in ~/.config/pan/prompts/
    extra_body: { ... }                  # provider settings passed through untouched
    concurrency: 2                       # a ceiling; the window opens and closes with the server's answers
    enabled: true
  embed:  { url: ..., model: qwen3-vl-embedding-2b, concurrency: 2 }
  pose:   { url: ..., model: rtmw-x-l,  concurrency: 2 }
  sam3:   { url: ..., model: facebook/sam3, concurrency: 2, enabled: false }
```

Every stage is optional. A missing config file means one store at `~/.pan`
and no model stages.

### The store file, `<store>/pan.yml`

Today it holds one line, the store's id. **Planned** shape:

```yaml
storage_id: "0000000000000000000000000000000000000000"
media: in-place                          # or a path; absent = the daemon's media volume
stages: [caption, embed]                 # which of the daemon's stages run on this store
prompts:
  caption: prompts/caption.md            # a file beside this one, or a kit-shipped prompt
backfill_since: none                     # this store wants everything captioned
ontologies: [pan]                        # what vocabulary a model answer may use here
```

A soul store gets its id from the repo and its vocabulary from the kits
installed in the repo, so it needs none of the planned lines.

### How a model answer becomes metadata

The caption prompt asks the model for one JSON object whose keys are Pan
property names: `shortDescription`, `longDescription`, `sceneObjects` (a
list), and the twelve scene fields (`sceneCamera` … `sceneLocation`). Pan
writes them onto the image, in the graph and in the XMP, and refuses the
whole answer if a key is not declared in pan.ttl. The prompt is the schema.
The model's raw answer is kept verbatim in the caption record beside the
image.

The order of the stages follows from the data. Segmentation is prompted with
`sceneObjects`, and the embedding is built from the image and its complete
XMP together, so both wait until the caption stage has written its fields.
Pose needs nothing and runs at once.
