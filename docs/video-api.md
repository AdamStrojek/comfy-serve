# Video generation API

The API is **asynchronous**. Generating a video takes from tens of seconds to several minutes, so it works in three steps:

1. **Submit the job.** `POST /api/v1/videos` returns the job `id` straight away (`202 Accepted`).
2. **Check the status.** Poll `GET /api/v1/videos/{id}` every few seconds until the status is `completed` (or `failed` / `cancelled`).
3. **Download the video.** `GET /api/v1/videos/{id}/content`.

**Files (images, audio, videos) are sent directly in the first request as `multipart/form-data`.** You don't need base64 or hosting the files anywhere.

The server does not remember the job for you. **Store the `id`**, because it is the only way to find the job again.

In the examples, `BASE=http://127.0.0.1:3000`.

---

## Authorization

If the server has API keys configured, every request needs this header:

```
Authorization: Bearer <your-key>
```

Without a valid key, the server answers `401 Unauthorized`. If keys are not configured, the header is not needed.

---

## 1. Submit a job: `POST /api/v1/videos`

### Request format

`multipart/form-data` with these fields:

| Field | Type | Description |
|---|---|---|
| `request` | JSON text (**required**) | Generation parameters, see the table below. |
| `first_frame` | file (image), max 1 | The first frame of the video. |
| `last_frame` | file (image), max 1 | The last frame of the video. |
| `image` | file (image), repeatable | Reference image (e.g. a character, an object, a style). |
| `audio` | file (audio), repeatable | Reference audio (e.g. speech, music). |
| `video` | file (video), repeatable | Reference video. |

Repeated fields may also be named `image[]`, `audio[]` and `video[]`. The order of files of the same kind is preserved. Any other field name causes an error.

There are two modes, and they **cannot be mixed in one request**:

- **Frames**: `first_frame` and/or `last_frame`. The video starts and/or ends with the given image.
- **References**: `image`, `audio`, `video` in any number. The model uses them as material for the scene (the look of characters, voice, motion).

Without any files, you get a video from text alone (text-to-video). In that case you can also send a plain `application/json` body (the same JSON that would go in `request`).

### Parameters (JSON in `request`)

| Field | Type | Default | Description |
|---|---|---|---|
| `model` | string (**required**) | – | Model name, see [`GET /api/v1/videos/models`](#5-list-models-get-apiv1videosmodels). |
| `prompt` | string (**required**) | – | Description of the video. |
| `duration` | integer | `5` | Length in seconds: **a whole number from 5 to 15**. |
| `resolution` | string | `"480p"` | `360p`, `480p`, `720p`, `768p`, `1080p`. This is the length of the shorter side. |
| `aspect_ratio` | string | `"16:9"` | `16:9`, `9:16`, `1:1`, `4:3`, `3:4`, `3:2`, `2:3`, `21:9`, `9:21` |
| `size` | string | – | Exact size `WIDTHxHEIGHT`, e.g. `"1280x720"`, **instead of** `resolution` + `aspect_ratio`. Sides are rounded to multiples of 64. |
| `seed` | integer | random | The same seed with the same parameters gives a repeatable result. |
| `video_sound` | array of integers | `[]` | Uses the **sound** of the given reference videos (numbered from 0, in the order of the `video` fields) as extra audio references. |

The final width and height are always multiples of 64. For example, `480p` + `16:9` gives 832×512, and `720p` + `9:16` gives 704×1280.

### Response: `202 Accepted`

```json
{
  "id": "eda0364a-04bf-433c-8c50-b0ba0d69a221",
  "polling_url": "http://127.0.0.1:3000/api/v1/videos/eda0364a-04bf-433c-8c50-b0ba0d69a221",
  "status": "pending"
}
```

`polling_url` is the ready-made address for checking the status.

### Examples

**Text only (JSON):**

```bash
curl -X POST "$BASE/api/v1/videos" \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
        "model": "minimax_h3_fast",
        "prompt": "A red fox walks through fresh snow at dawn, cinematic",
        "duration": 5,
        "resolution": "720p",
        "aspect_ratio": "16:9"
      }'
```

**Text only (multipart):**

```bash
curl -X POST "$BASE/api/v1/videos" \
  -H "Authorization: Bearer $API_KEY" \
  -F 'request={"model":"minimax_h3_fast","prompt":"A red fox walks through fresh snow"};type=application/json'
```

**First frame (image-to-video):**

```bash
curl -X POST "$BASE/api/v1/videos" \
  -H "Authorization: Bearer $API_KEY" \
  -F 'request={"model":"minimax_h3","prompt":"The camera slowly pulls back, the woman turns and smiles","duration":8,"aspect_ratio":"9:16"};type=application/json' \
  -F first_frame=@portrait.png
```

**First and last frame (a transition between two images):**

```bash
curl -X POST "$BASE/api/v1/videos" \
  -H "Authorization: Bearer $API_KEY" \
  -F 'request={"model":"minimax_h3","prompt":"Day turns into night over the city","duration":10};type=application/json' \
  -F first_frame=@city_day.jpg \
  -F last_frame=@city_night.jpg
```

**References: two images plus a voice recording:**

```bash
curl -X POST "$BASE/api/v1/videos" \
  -H "Authorization: Bearer $API_KEY" \
  -F 'request={"model":"minimax_h3","prompt":"The man from the first image talks to the dog from the second image, speaking the line from the audio","duration":7};type=application/json' \
  -F image=@man.png \
  -F image=@dog.png \
  -F audio=@line.wav
```

**Reference video together with its sound (`video_sound`):**

```bash
curl -X POST "$BASE/api/v1/videos" \
  -H "Authorization: Bearer $API_KEY" \
  -F 'request={"model":"minimax_h3","prompt":"The same dance, performed by a robot on the Moon","video_sound":[0]};type=application/json' \
  -F video=@dance.mp4 \
  -F image=@robot.png
```

Here, `"video_sound":[0]` means the sound of the first video (`dance.mp4`) is also used as an audio reference.

---

## 2. Check the status: `GET /api/v1/videos/{id}`

```bash
curl "$BASE/api/v1/videos/eda0364a-04bf-433c-8c50-b0ba0d69a221" \
  -H "Authorization: Bearer $API_KEY"
```

### Response: `200 OK`

```json
{
  "id": "eda0364a-04bf-433c-8c50-b0ba0d69a221",
  "polling_url": "http://127.0.0.1:3000/api/v1/videos/eda0364a-04bf-433c-8c50-b0ba0d69a221",
  "status": "completed",
  "unsigned_urls": [
    "http://127.0.0.1:3000/api/v1/videos/eda0364a-04bf-433c-8c50-b0ba0d69a221/content?index=0"
  ],
  "error": null
}
```

| `status` | Meaning | What to do |
|---|---|---|
| `pending` | Waiting in the queue | Check again later |
| `in_progress` | Generating | Check again later |
| `completed` | Ready, and `unsigned_urls` holds the download links | Download the video |
| `failed` | Failed, the reason is in `error` | Fix the request and submit again |
| `cancelled` | Interrupted while generating | – |

Checking every 5–30 seconds is reasonable.

---

## 3. Download the video: `GET /api/v1/videos/{id}/content`

```bash
curl "$BASE/api/v1/videos/eda0364a-04bf-433c-8c50-b0ba0d69a221/content" \
  -H "Authorization: Bearer $API_KEY" \
  -o video.mp4
```

- It returns the video file (usually `video/mp4`, with sound).
- `?index=N` picks the N-th video if the job produced several. The default is `0`. The easiest way is to use the links from `unsigned_urls`.
- If the job is not finished yet, you get `409 Conflict`.

---

## 4. Cancel a job: `DELETE /api/v1/videos/{id}`

Removes a waiting job from the queue or interrupts one that is generating.

```bash
curl -X DELETE "$BASE/api/v1/videos/eda0364a-04bf-433c-8c50-b0ba0d69a221" \
  -H "Authorization: Bearer $API_KEY"
```

```json
{ "id": "eda0364a-04bf-433c-8c50-b0ba0d69a221", "status": "cancelled" }
```

- A job that has already finished cannot be cancelled. That returns `409 Conflict`.
- A job removed **before it started** disappears completely, so a later status check for it returns `404`. A job interrupted **while generating** gets the status `cancelled`.

---

## 5. List models: `GET /api/v1/videos/models`

```bash
curl "$BASE/api/v1/videos/models" -H "Authorization: Bearer $API_KEY"
```

```json
{
  "data": [
    {
      "id": "minimax_h3_fast",
      "name": "minimax_h3_fast",
      "created": 1792743253,
      "supported_durations": [5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
      "supported_resolutions": ["360p", "480p", "720p", "768p", "1080p"],
      "supported_aspect_ratios": ["16:9", "9:16", "1:1", "4:3", "3:4", "3:2", "2:3", "21:9", "9:21"],
      "supported_inputs": ["first_frame", "last_frame", "image", "audio", "video"]
    }
  ]
}
```

Use the value of `id` as `model` when submitting a job.

---

## Errors

Every error has the same format:

```json
{ "error": { "code": 400, "message": "duration must be a whole number of seconds from 5 to 15" } }
```

| Code | When |
|---|---|
| `400` | Invalid parameters: missing `model` or `prompt`, an unknown model, `duration` outside 5–15 or not a whole number, an unsupported `resolution` or `aspect_ratio`, frames mixed with references, an unknown multipart field, or `video_sound` pointing to a video that was not sent. |
| `401` | Missing or invalid API key. |
| `404` | Unknown job `id`, or the server no longer remembers the job (see below). |
| `409` | Downloading a video that is not ready yet, or cancelling a job that has already finished. |
| `502` | The generation backend is unavailable or rejected the job. |

---

## Good to know

- **Download the video soon after it is ready.** The server does not keep finished jobs indefinitely. After the generation backend is restarted or its history is cleared, the `id` stops working (`404`).
- **Keep the files small.** Reference files are sent whole in the first request, so a long reference video makes the upload take longer.

---

## Example: a complete script (bash)

```bash
#!/usr/bin/env bash
set -euo pipefail
BASE=http://127.0.0.1:3000
AUTH="Authorization: Bearer $API_KEY"

# 1. Submit
ID=$(curl -s -X POST "$BASE/api/v1/videos" -H "$AUTH" \
  -F 'request={"model":"minimax_h3_fast","prompt":"A paper boat floats down a rainy street","duration":6};type=application/json' \
  -F first_frame=@boat.png | jq -r .id)
echo "Job: $ID"

# 2. Wait
while true; do
  RES=$(curl -s "$BASE/api/v1/videos/$ID" -H "$AUTH")
  STATUS=$(echo "$RES" | jq -r .status)
  echo "Status: $STATUS"
  case "$STATUS" in
    completed) break ;;
    failed|cancelled) echo "$RES" | jq -r .error; exit 1 ;;
  esac
  sleep 10
done

# 3. Download
curl -s "$BASE/api/v1/videos/$ID/content" -H "$AUTH" -o "$ID.mp4"
echo "Saved: $ID.mp4"
```

## Example: Python (`httpx`)

```python
import json, time, httpx

BASE = "http://127.0.0.1:3000/api/v1/videos"
headers = {"Authorization": "Bearer YOUR_KEY"}

params = {"model": "minimax_h3", "prompt": "The man from the image waves", "duration": 5}
with open("man.png", "rb") as image, open("hello.wav", "rb") as audio:
    r = httpx.post(
        BASE,
        headers=headers,
        files=[
            ("request", (None, json.dumps(params), "application/json")),
            ("image", ("man.png", image, "image/png")),
            ("audio", ("hello.wav", audio, "audio/wav")),
        ],
    )
r.raise_for_status()
job = r.json()

while (job := httpx.get(job["polling_url"], headers=headers).json())["status"] in ("pending", "in_progress"):
    time.sleep(10)

if job["status"] != "completed":
    raise RuntimeError(job["error"])

video = httpx.get(job["unsigned_urls"][0], headers=headers, timeout=300)
open("out.mp4", "wb").write(video.content)
```
