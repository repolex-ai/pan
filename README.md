# Pan

Pan is a local, sovereign media store and perception engine designed for private storage, state-of-the-art knowledge graph retrieval, and multi-modal AI enrichment. It can be used as an independent standalone media store or as the visual storage layer of the Subtexture Stack.

## Installation

```sh
git clone https://github.com/repolex-ai/pan.git
cd pan && cargo install --path .
```

---

## Quick Start

```sh
# 1. Start the daemon (runs in foreground, manages stores and HTTP API)
pand start

# 2. In another terminal, store an image
pan store ~/Pictures/sample.png
# → <https://repolex.ai/pan/Image/k7m2p9x4>

# 3. Inspect image state and enrichment progress
pan state '<https://repolex.ai/pan/Image/k7m2p9x4>'
pan info  '<https://repolex.ai/pan/Image/k7m2p9x4>'

# 4. Query the knowledge graph via SPARQL
pan query 'SELECT ?s ?r WHERE { ?s pan:rating ?r . FILTER(?r >= 4) }'

# 5. Check daemon health and registered stores
pand status
pan stores

# 6. Open interactive API docs
open http://127.0.0.1:7401/swagger-ui
```

---

## Overview

Modern creative workflows and generative AI pipelines produce thousands of high-resolution images, while photographers manage multi-terabyte archives spanning millions of camera RAW captures. Existing options force an uncomfortable trade-off: either keep "dumb folders" on disk with zero semantic searchability, or upload sensitive assets to closed cloud DAM platforms with recurring subscription fees, privacy risks, and bandwidth bottlenecks.

Pan bridges this gap by turning any local directory or external photographic volume into a **sovereign, queryable visual knowledge base**. It combines embedded open-standard metadata (XMP/RDF), an embedded W3C SPARQL graph database (Oxigraph), fast local vector indexing (USearch HNSW), and an asynchronous perception ladder powered by local and resident AI vision models.

---

## Key Features & Selling Points

* **100% Local & Sovereign:** Your master files, graph triples, and vector embeddings remain on your own disks. Zero telemetry, zero external accounts, and zero cloud lock-in.
* **Hybrid Graph & Vector Retrieval:** Combines structured metadata filters (ratings, camera EXIF, shoot dates, custom tags) with semantic embeddings in a single search pass using Oxigraph SPARQL and USearch HNSW.
* **Two Complementary Storage Modes:**
  * **Managed Store (Mode 1):** Lossless compressed master storage for generated and active project assets, saving rich RDF metadata directly inside standard PNG XMP chunks.
  * **Referenced Archive Indexer (Mode 2):** Indexes multi-terabyte archives (1.2M+ RAW/DNG files) directly on external SSDs and hard drives without moving or copying master files, caching fast 4K display previews and 512px thumbnails locally.
* **Resident Multi-Modal AI Perception Ladder:** Asynchronous, non-blocking background workers continuously enrich indexed assets:
  * **Dense Semantic Embeddings:** Multi-modal vectors for natural language search (`qwen3-vl-embedding-2b`).
  * **Structured Captions & Aesthetics:** High-fidelity scene descriptions, technical clarity scores, and artistic critiques.
  * **Kinematics & Pose Estimation:** 133-keypoint human posture matching (`rtmw-pose`), searching body geometry invariant to wardrobe or background.
  * **Spatial Depth Profiles:** Monocular metric depth geometry and focal plane analysis (`depth-anything-v2-base`).
  * **Zero-Shot Face Identity:** 512-dim facial identity vectors (`insightface`) for clustering individuals across historical archives.
  * **Instant Duplicate Triage:** 64-bit gradient difference hashing (dHash) for zero-cost duplicate detection.
* **Disk-First Truth & Resilient Architecture:** All catalog metadata lives in flat XML/XMP sidecars and Oxigraph N-Quads on disk. External disk unplugs or workstation crashes never corrupt your library; the ephemeral crawler queue resumes instantly.
* **Dual-Control Interface Ready:** Built to pair directly with `pan-ui` for 120fps human culling and live AI agent viewport steering via `pansee`.

---

## Architecture

Pan consists of two binaries compiled from a single crate:

* **`pand` (The Daemon):** One process per machine. It is the sole component that reads and writes store files, commits transactions to Oxigraph, manages USearch HNSW vector indexes, runs async background perception stages (embeddings, captions, poses, depth), and serves the Axum HTTP/SSE API (default port `7401`).
* **`pan` (The CLI):** A thin command-line client that communicates with `pand` over HTTP. Every answer it displays is queried directly from the graph; it never reads store files directly.

---

## Configuration (`~/.config/pan/config.yml`)

Pan is configured via a single YAML file. If absent, `pand` defaults to a single local store at `~/.pan` on port `7401` with no external model stages.

```yaml
stores:
  - ~/projects/creative-studio/.pan      # Project-local store
  - ~/.pan                               # Standalone workstation store
default: ~/projects/creative-studio/.pan
port: 7401
interval_secs: 5                         # Polling interval between background worker passes
batch: 8                                 # Images per stage per pass
models:                                  # External perception model stages (optional)
  embed:
    url: http://127.0.0.1:1215/see_embed
    model: qwen3-vl-embedding-2b-8bit
    caption_model: qwen3.5-9b-mlx-8bit
    concurrency: 1
  pose:
    url: http://127.0.0.1:1215/see_pose
    model: rtmw-x-l
    enabled: false                       # Staged, but inactive until enabled
```

---

## Storage & Ingestion Pipeline

### Operational Modes

1. **Mode 1 (Managed Store):** Used for newly generated assets, generative AI renders, and active project media. Files are stored losslessly as compressed PNGs in `media/image/YYYY/MM/DD/` with metadata written directly into standard PNG XMP chunks.
2. **Mode 2 (Referenced Indexer):** Used for large photographic archives (e.g. 1.2M camera RAW/DNG files). Master RAW files remain untouched on external volumes; Pan extracts 4K JPEG previews (`pan:previewImage`) and indexes EXIF/XMP metadata into the local graph.

### Ingestion Sequence

1. **XMP Harvest:** Incoming image metadata is parsed using an RDF/XML parser (`rdf:about=""` binds to the new image node).
2. **Disk Storage:** Image bytes land in the store path, and Pan appends its own identity and enrichment block to the XMP packet.
3. **Thumbnail Generation:** 512px square-padded JPEG thumbnails are generated for fast preview.
4. **Atomic Graph Commit:** Statements and file records are committed to Oxigraph in a single atomic transaction.
5. **Background Stage Ladder:** `pand` queries the graph for images lacking configured model outputs, invokes models via bounded HTTP worker pools, and saves vectors (`.npy`) and overlays (`.xml`/`.png`).

---

## Store Layout

```
<root>/                       # e.g., ~/projects/my-project/.pan or ~/.pan
  pan.yml                     # Storage ID (for standalone stores)
  _ignore/                    # Gitignored runtime data
    pan.ttl                   # Reference copy of the Pan ontology
    oxigraph/                 # Oxigraph embedded RDF database
    hnsw/<model>/             # USearch HNSW vector index
    media/                    # Media assets (or symlink to external volume)
      image/YYYY/MM/DD/YYYYMMDD-HHMMSS-<id>.png
      thumbnail/YYYY/MM/DD/YYYYMMDD-HHMMSS-<id>.jpg
      vectors/<model>/<id>.npy
      caption/YYYY/MM/DD/<id>.<model>.xml
      pose/YYYY/MM/DD/<id>.xml
```

With `media_volume: /Volumes/p02/_pan`, the media root moves to `/Volumes/p02/_pan/<store_id_prefix>/media`. The root's absolute path is declared in the store graph as `pan:mediaRoot` on startup.

---

## ExifTool Integration

Pan metadata is stored in standard XMP packets under the `pan` namespace. To view Pan-specific fields in desktop image viewers (such as Xee³):

```sh
cp exiftool/ExifTool_config ~/.ExifTool_config
```

This configures ExifTool to group Pan metadata into its own **Pan** section alongside standard EXIF and IPTC fields.
