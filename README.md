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

Pan bridges this gap by turning any local directory or external photographic volume into a **sovereign, queryable visual knowledge base**. It combines embedded open-standard metadata (XMP/RDF), an embedded graph database (Oxigraph), fast local vector search (USearch HNSW), and automatic background AI enrichment using local vision models.

---

## Key Features

* **100% Local & Sovereign:** Your master files, catalog records, and search vectors remain on your own disks. Zero telemetry, zero external accounts, and zero cloud lock-in.
* **Primary Media Store or Referenced Index:** Pan can act as the primary, managed store for your media files, or it can run purely as an index keeping your original media in place. This is especially useful if you are already committed to existing asset management software (such as Lightroom, Capture One, or Apple Photos) or an established folder structure, but want to take advantage of Pan's graph and AI retrieval features without moving or duplicating terabytes of data.
* **Unified Graph & Visual Search:** Traditional catalogs only let you search rigid metadata tags (e.g. 5-star rating, 85mm lens, 2024), while modern AI tools only let you search by generic visual vibes. Pan combines both in a single query:
  * *"Find 5-star studio portraits shot on an 85mm lens that have dramatic rim lighting and a pose similar to this reference photo."*
  * *"Find every unpicked outtake of Model Sarah from last year's shoots where the subject is mid-jump."*
  * *"Find photos where two different vision models disagreed on aesthetic appeal, filtered to black-and-white images."*
* **Continuous Background AI Analysis:** Pan automatically enriches images in the background using local and resident vision models:
  * **Natural Language Descriptions:** Detailed scene captions, technical clarity scores, and artistic critiques.
  * **Human Pose & Kinematics:** Recognizes body posture and skeletal geometry (`rtmw-pose`), letting you find matching poses regardless of wardrobe, model, or background.
  * **3D Depth & Spatial Geometry:** Understands focal planes, depth-of-field, and subject isolation (`depth-anything-v2-base`).
  * **Face Recognition:** Identifies and groups the same person across years of historical shoots without manual tagging (`insightface`).
  * **Visual Similarity & Duplicate Detection:** Finds visually similar frames and flags near-duplicate burst shots instantly.
* **LoRA & Fine-Tuning Dataset Curation & Evaluation:** Curate, prep, track, and evaluate model training runs from a single unified system. Assemble training candidates into Photosets, generate rich captions and descriptive tags, track model checkpoints, and **directly compare generated synthetic renders side-by-side with your actual ground-truth source photos** to verify subject likeness, lighting transfer, and anatomical fidelity.
* **Crash-Proof & Drive-Friendly:** All metadata is stored in standard industry-standard formats (open XMP sidecars and local graph files) directly alongside your media. Unplugging an external hard drive mid-scan or rebooting your computer will never corrupt your catalog; Pan resumes indexing automatically right where it left off.
* **Instant Culling & Agent Dual-Control:** Pairs directly with `pan-ui` for ultra-fast keyboard-driven photo review and culling. Autonomous AI agents can also steer the screen in real-time (`pansee`) to present visual search results directly to you.

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

### Flexible Storage Options

* **Managed Primary Store:** When Pan manages your files directly (such as newly rendered assets or active projects), files are stored losslessly as compressed PNGs in a clean date-based folder structure, with open XMP metadata embedded directly into the files.
* **Referenced Archive Index:** When indexing an existing library (such as large external hard drives or multi-terabyte camera RAW/DNG collections), Pan leaves your original files completely untouched. It extracts fast 4K previews and thumbnails to its local cache, indexing all EXIF, ratings, and camera metadata into the local graph.

### Ingestion Sequence

1. **XMP Harvest:** Incoming image metadata is parsed using an RDF/XML parser (`rdf:about=""` binds to the new image node).
2. **Disk Storage:** Image bytes land in the store path, and Pan appends its own identity and enrichment block to the XMP packet.
3. **Thumbnail Generation:** 512px square-padded JPEG thumbnails are generated for fast preview.
4. **Atomic Graph Commit:** Statements and file records are committed to Oxigraph in a single atomic transaction.
5. **Background Stage Ladder:** `pand` queries the graph for images lacking configured model outputs, invokes models via bounded HTTP worker pools, and saves vectors (`.npy`) and overlays (`.xml`/`.png`).

---

## Store Layout

```
<root>/                                       # e.g., ~/projects/my-project/.pan or ~/.pan
├── pan.yml                                   # Storage ID (for standalone stores)
└── _ignore/                                  # Gitignored runtime data
    ├── oxigraph/                             # Oxigraph embedded RDF database
    ├── hnsw/                                 # Vector search indexes
    │   └── <model>/                          # USearch HNSW index per embedding model
    └── media/                                # Media assets (or symlink to external volume)
        ├── image/YYYY/MM/DD/YYYYMMDD-HHMMSS-<id>.png
        ├── thumbnail/YYYY/MM/DD/YYYYMMDD-HHMMSS-<id>.jpg
        ├── vectors/<model>/<id>.npy
        ├── caption/YYYY/MM/DD/<id>.<model>.xml
        └── pose/YYYY/MM/DD/<id>.xml
```
