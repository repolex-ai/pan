# The perception contract: what Pan sends and expects for embed, pose, segment, depth and face

Captions have an industry standard: Pan sends an OpenAI chat-completions
request and reads an OpenAI chat-completions answer, so any provider that
speaks it works. The other four model calls have no industry standard. This
document is Pan's own contract for them. Any server that implements it works
with Pan, and Pan expects nothing beyond it. Written 2026-10-08; the shapes
below are what pand sends today (`src/daemon/client.rs`).

## Common to all four

- `POST` to the stage's configured `url`, body `multipart/form-data`.
- One file part named `image`: the image bytes, with a file name ending in
  `.png` or `.jpg` and a matching `Content-Type` (`image/png`, `image/jpeg`).
  Embed gets the stored file as it is. Pose, segment, depth and face get a copy of
  its pixels as a JPEG at quality 90 with the same width and height (since
  2026-11-04), so the coordinates they return are in the stored image's
  pixel space and the upload is about a fifth of the size; a 3.5 MB PNG
  goes up as a 0.6 MB JPEG.
- Optional `Authorization: <auth>` header when the stage's config sets `auth`.
- The answer is `200` with a JSON object. Unknown fields are kept beside the
  record; a missing optional field is simply absent.
- `422` means the server refuses this image for good; Pan stops asking.
- Any other `4xx` or `5xx` means "not now": Pan asks again later. A server
  that cannot be reached at all holds the stage for a few seconds.
- A `200` with an empty or unreadable body is a failure, asked again later.
- Pan sends a fixed six requests at a time and never asks what the server
  can take.

## embed

Request fields: `image`, and `text`, the caption text the vector is built
from alongside the pixels (the short and long captions and the render
request, one `name: value` per line).

Answer:

```json
{"vector": [0.01, -0.02, ...], "dim": 2048,
 "model": "qwen3-vl-embedding-2b", "precision": "bf16", "provider": "salad"}
```

- `vector` (required): the embedding, floats. `dim` (optional) must equal
  its length when present.
- `model`, `precision`, `provider` (optional): recorded on the embedding.

## pose

Request fields: `image`, and `with_keypoints` = `true`.

Answer:

```json
{"keypoints": [[[x, y, confidence], ... 133 per person], ...],
 "skeleton_png_b64": "<base64 PNG>"}
```

- `keypoints`: one list per detected person, each a list of
  `[x, y, confidence]` in pixel coordinates, COCO-WholeBody order. An image
  with nobody in it returns an empty list.
- `skeleton_png_b64` (optional): the drawn skeleton overlay as a PNG.

## segment

Request fields: `image`; `prompts`, one comma-separated string of nouns to
find; `confidence`, the score a region must reach; `polygon_verts`, how many
vertices each outline is reduced to.

Answer:

```json
{"regions": [{"prompt": "person", "score": 0.93,
              "bbox": [x1, y1, x2, y2], "polygon": "x,y;x,y;..."}, ...]}
```

- `regions`: one entry per thing found, any number per noun, none for a
  noun that found nothing. `prompt` is the noun it answers; `score` the
  model's confidence; `bbox` four integers in pixels; `polygon` the outline
  as `x,y` pairs joined by `;`.
- Any other fields on the answer are kept whole beside the record.

## depth

Request fields: `image` only.

Answer:

```json
{"depth_png_b64": "<base64 PNG>", "min": 0.0031, "max": 0.98,
 "width": 1024, "height": 1536,
 "model": "depth-anything-v2-base", "precision": "fp16", "provider": "salad"}
```

- `depth_png_b64` (required): an 8-bit greyscale PNG the size of the image,
  255 nearest, 0 farthest.
- `min`, `max`: the relative depth range the map was normalised from, in
  the model's own units, larger meaning nearer; not metres.
- `width`, `height`, `model`, `precision`, `provider`: optional, recorded.

## face

Request fields: `image` only. Added 2026-11-09 (pan.ttl 0.4.21); the route
is Iris `POST /percept/face`, a pass-through to the face server on katoptron
(InsightFace buffalo_l, CPU, about a quarter second an image).

Answer:

```json
{"faces": [{"bbox": [323.5, 159.3, 711.7, 620.8], "score": 0.8149,
            "kps": [[476.5, 335.5], [647.5, 364.1], [579.3, 446.2], [442.2, 472.7], [624.0, 497.3]],
            "pose": [-1.08, 18.82, 11.37], "age": 29, "sex": "F",
            "landmark_2d_106": [[x, y], "... 106 points"], "landmark_3d_68": [[x, y, z], "... 68 points"],
            "embedding": ["... 512 floats, unit length"]}],
 "width": 1024, "height": 1536,
 "model": "insightface/buffalo_l", "precision": "fp32-onnx-cpu", "provider": "katoptron"}
```

- `faces` (required): every face found, largest first; `[]` when there is
  none. A body with no `faces` key is the node failing and the image stays
  pending.
- Per face, what the graph records: `bbox` four pixel numbers; `score`;
  `kps` five `[x, y]` points, in the model's order left eye, right eye, nose,
  mouth left, mouth right (pan:landmarks); `pose` pitch, yaw, roll in degrees
  (pan:headPose); `embedding`, written to a `.npy` beside the record and into
  the face index under the Face record's id, never into the graph.
- `age`, `sex`, `landmark_2d_106`, `landmark_3d_68` and anything else: kept
  whole in the saved answer beside the record, not read into the graph.
- `width`, `height`, `model`, `precision`, `provider`: optional, recorded.
