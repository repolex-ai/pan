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
    ├── img/                               pixels: the pictures and their renditions
    │   ├── original/YYYY/MM/DD/           what arrived, when it was not PNG; kept, never read again
    │   │   └── 20260907-043526-v5ha2dfd.jpg
    │   ├── source/YYYY/MM/DD/             THE image: always PNG, XMP written in, what every stage reads
    │   │   └── 20260907-043526-v5ha2dfd.png
    │   └── jpg/YYYY/MM/DD/                derived JPEG sizes, named by long edge; the thumbnail is _512
    │       └── 20260907-043526-v5ha2dfd_512.jpg
    └── enrichment/                        model output: records about the picture
        ├── caption/YYYY/MM/DD/            one N-Quads record per model run, the raw answer inside
        ├── segment/YYYY/MM/DD/            regions per noun (bbox, polygon, score), plus the server's answer as .json
        ├── pose/YYYY/MM/DD/               keypoints per person, plus an overlay image
        ├── depth/YYYY/MM/DD/              one depth map per image, plus the server's answer as .json
        └── embed/YYYY/MM/DD/              one .npy per image, plus the server's answer as .json

<store root>/                          the store itself
├── pan.yml                            the store's own settings (today: its id)
└── _ignore/
    ├── ImageSet/                      one N-Quads file per set
    ├── oxigraph/                      the graph: every fact about every file
    └── hnsw/<model>/                  the vector index, one per embedding model
```

A bare store puts its media root inside `_ignore/media/`. When a media volume
is configured, the media root is `<volume>/_pan/<first 6 chars of the store
id>/pan/` and the folder above is what you find there. The full layout, with
the naming rules, is in `docs/2026_09_21_PAN_FILE_LAYOUT.md`.

The source image is always PNG. A JPEG, WebP, GIF or TIFF that arrives is
converted once, and the bytes as delivered are kept under `img/original/`.

The file name of a stored image is `<local date>-<local time>-<id>.<ext>`.
The id is the last eight characters and is the same id the graph uses:
`<pan/Image/v5ha2dfd>`.

The image's XMP carries everything Pan knows about it under the `pan`
namespace: id, creation time, path, type, the source file it was made from, size, the short and long
descriptions, the scene objects, the scene fields, the thumbnail, and one
reference per model run. What a producer wrote into the file before it
arrived is kept untouched, and its statements load into the graph as written.

Two `pan` fields a producer may write are honoured on arrival; every other
`pan` statement in an arriving file belongs to a previous store and stays
out. `pan:relatedToId` names what the image belongs to, a Moment or a set.
`pan:mediaCreatedDate` says when the media itself was made, as an RFC3339
date with its zone; a file whose value is not one is refused whole. A
`pan:relatedToId` naming `<pan/ImageSet/id>` that does not exist yet makes
the set, with that id, as the image lands.

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
| `~/.config/pan/logs/calls/YYYY-MM-DD.jsonl` | this machine | the model-call log: one JSON line per call, by local day; files older than `log_keep_days` are removed at start and once a day |
| `<store>/pan.yml` | one store | the store's id; **planned:** its media root, which stages run, which prompt file each uses, its backfill floor, which ontologies apply |
| `<store>/_ignore/` | one store | the graph and the vector index; never edited by hand |
| `~/.config/pan/ontology/pan.ttl` | this machine | the ontology the running pand was built with, rewritten at every start; read it, do not edit it |
| `~/.pan/logs/pand.log` | this machine | the daemon's log, also printed in the terminal that started it |

### The daemon file, `~/.config/pan/config.yml`

```yaml
stores:                                  # every store this daemon serves, in priority order
  - /Users/me/repos/squad/agent-a        # a soul repo: store at <repo>/.pan, id = the repo's genesis SHA
  - ~/.pan                               # a bare store: id from its own pan.yml
default: ~/.pan                          # where `pan store <file>` lands when no store is named
media_volume: /Volumes/p02/_pan          # media root = <volume>/<6-char id>/pan; omit to keep media in the store
port: 7401
backfill_since: "2026-09-07T04:15:45-07:00"   # images created before this are not sent to models
log_keep_days: 30                        # days of model-call log files kept under logs/calls/
models:
  caption:
    url: http://127.0.0.1:1215/percept/vlm
    model: qwen/qwen3.8-27b              # the provider's own name, sent as written; Pan labels records qwen3-8-27b
    prompt: caption.md                   # a file in ~/.config/pan/prompts/
    extra_body: { ... }                  # provider settings passed through untouched
    enabled: true
  embed:  { url: ..., model: qwen3-vl-embedding-2b }
  pose:   { url: ..., model: rtmw-x-l }
  segment: { url: ..., model: sam3, enabled: false }
```

Every stage is optional. A missing config file means one store at `~/.pan`
and no model stages.

A pass sends four images to the stage's one address at once and records
what comes back, then rests five seconds; that is Pan's own pace, the same
for every server. Pan does not count what the server can take and
has no second address for a stage: a call the server refuses (429, 503, a
timeout) is asked for again on a later pass, and a server that cannot be
reached at all holds the stage for a few seconds before the next try
(goodlux, 2026-10-08).

### The model-call log, `~/.config/pan/logs/calls/`

Every call pand makes to a model writes one JSON line to the file for the
current local day. The line holds what happened, never what was sent or
said: no image bytes, no answer text.

```json
{"time":"2026-09-16T10:42:07.318-07:00","store":"700c5bd4a969723107c1b92b83c0f1ec1497d9d4","id":"ygjjmvkw","stage":"caption","model":"qwen/qwen3.8-27b","url":"http://127.0.0.1:1215/percept/vlm","request_bytes":812344,"status":200,"latency_ms":9412,"response_bytes":3120,"outcome":"recorded","error":null}
```

- `status` is the HTTP status, or null when no answer came back at all.
- `request_bytes` is the payload (image plus text or JSON body), not the
  wire size.
- `outcome` is one of `recorded` (the answer was written), `unreachable` (the
  server could not be reached, the stage held), `quota` (the provider account is out of credit, the
  stage held), `transient` (asked again later), `terminal` (this image is
  never asked again by this stage).

`log_keep_days` in `config.yml` says how many days of files to keep; absent
means 30, and 0 is refused. A day's cost is one `jq` away:

```sh
jq -r 'select(.outcome=="recorded") | .stage' ~/.config/pan/logs/calls/2026-09-16.jsonl | sort | uniq -c
```

A log write that fails is a warning in the daemon log, once; it never stops
a stage. The daemon's own log, `~/.pan/logs/pand.log`, is unchanged.

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

### Setting facts by hand

Three facts on an image belong to a person, not a model: a star rating, a
pick, and a reject. They are written onto the image like every other pan
fact, into the graph and into the file's XMP, so a rating travels with the
file and needs no second database (goodlux, 2026-09-16).

```sh
pan set   '<pan/Image/k7m2p9x4>' rating=4 isPicked=true
pan unset '<pan/Image/k7m2p9x4>' rating
```

| property | value | meaning |
|---|---|---|
| `rating` | whole number 0 to 5 | star rating |
| `isPicked` | `true` or `false` | a pick |
| `isRejected` | `true` or `false` | a reject |

Setting overwrites: an image has at most one of each. Over HTTP the same
request is `POST /media/{id}/set` with one JSON object keyed by property name,
and `POST /media/{id}/unset` with a list of names. The three above are the
usual ones, but any property pan.ttl declares on an image can be set the
same way, including the captions and scene fields the caption stage writes
and the facts pand records at ingest. pand checks every key against pan.ttl
before writing anything: a name the ontology does not declare is refused
with the list of what it does declare, and a value outside the declared
range (a rating of 6, a pick of `yes`) is refused naming the expected type.
A new field is declared in pan.ttl first; the command reads the list from
the ontology.

### Running one stage again for one image

```sh
pan redo '<pan/Image/k7m2p9x4>' embed
```

forgets that stage's result for that image: the reference, its records,
the files they name (record, server reply, vector, depth map, overlays,
masks), the vector index entry for embed, and the completion date. The
image's XMP is rewritten without the reference, and the stage picks the
image up again on its next pass. The caption's own fields stay on the image
when the embed stage is redone; redoing caption replaces them when the new
caption lands. Over HTTP: `POST /media/{id}/redo` with `{"stage": "embed"}`;
stages are caption, embed, segment, pose, depth.

### Moving facts from one namespace to another

A producer's facts are loaded as written, so a producer that changes the
spelling of its namespace leaves a store with the same field under two
IRIs. `POST /stores/{id}/rename-namespace` with `{"from": "…", "to": "…"}`
moves every fact in that store whose predicate (or IRI value) sits under
`from` to the same name under `to`, in the graph and in each image's XMP,
one image at a time. The answer says how many images were rewritten and how
many facts moved; running it again does nothing. An image whose file cannot
be rewritten has its facts put back and the call stops there, so no image
disagrees with its own file. Added for the copia facts that arrived under
`https://repolex.ai/ontology/kit/copia/` before the namespace settled on
`https://repolex.ai/ontology/copia/` (goodlux, 2026-10-03).

### How a model answer becomes metadata

The caption prompt asks the model for one JSON object whose keys are Pan
property names: `shortCaption`, `longCaption`, `sceneObjects` (a
list), and the twelve scene fields (`sceneCamera` … `sceneLocation`). Pan
writes them onto the image, in the graph and in the XMP, and refuses the
whole answer if a key is not declared in pan.ttl. The prompt is the schema.
The model's raw answer is kept verbatim in the caption record beside the
image.

The order of the stages follows from the data. Segmentation is prompted with
`sceneObjects`, and the embedding is built from the image together with its
two captions and the render request the file arrived with (the scene fields,
scores and critiques stay out of it), so both wait until the caption stage
has written its fields.
Pose needs nothing and runs at once.
