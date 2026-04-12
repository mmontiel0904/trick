# Video Frame Extractor API

A highly optimized Rust API built with Axum that accepts a video file alongside a specific timestamp and returns an exact frame extract (`.jpg`). It streams uploaded videos directly to your disk avoiding out-of-memory (OOM) situations on tight environments, uses FFmpeg directly via shell integration, and ships with an embedded SQLite usage & quota protection system.

## Features

- **Blazing Fast & Low Memory Footprint**: Instead of loading the incoming video payload entirely into RAM, `multipart` chunks are saved directly onto the server disk as they are received. Allowing parsing of 500MB+ videos perfectly fine using barely any RAM.
- **Auto Documentation**: Ships with native OpenAPI Swagger UI endpoints. Perfect for AI tools or your preferred platform (e.g., `n8n`) to digest schema documentation out-of-the-box.
- **Embedded API Usage Control**: Built-in SQLite db tracks `X-API-Key` requests, rejecting them dynamically if they surpass their individual maximum request quota limits.
- **Ready for Railway Deploy**: Configured alongside a pure lightweight `railpack.json` configuration file, utilizing Railway's bleeding-edge Railpack system to compile the Rust binary automatically and inject `FFmpeg` packages at runtime—zero Dockerfiles needed!

## Quick Start (Deployment on Railway)

1. Push this repository to Github, or use the `railway up` CLI on this directory.
2. Railway detects your project and utilizes our `railpack.json` instruction to deploy it instantly with native apt packages (`ffmpeg`, `sqlite3`).
3. Establish a **Railway Volume** and map it to `/app/data`. This causes the internal API Key Database file (`data.db`) to persist across restars. 
4. Set the following environmental variables: 
    * `PORT`: Server's HTTP Binding Port (Native `3000` recommended).
    * `ADMIN_KEY`: Secret string securing your key generation endpoint.
    * `DATABASE_URL`: Connection string. On Railway, this should be `sqlite:///app/data/data.db?mode=rwc`. Locally, it defaults naturally to `sqlite:data.db?mode=rwc`.

You can also rely on `.env.example` if deploying locally.

## Usage & API Interaction

You can interactive natively with all endpoints including testing inside your browser by heading to **`http://YOUR-DOMAIN/swagger-ui`**. 

### 1. Generate an API Key

Before processing videos, you need to generate an API key tied to a specific request quota limit. An admin user with the `ADMIN_KEY` environment variable secret can issue keys using a simple POST.

```bash
curl -X POST http://YOUR-DOMAIN/admin/keys \
  -H "x-admin-key: your_secure_password" \
  -H "Content-Type: application/json" \
  -d '{"max_quota": 500}'
```

Returns:
```json
{
  "key": "trick-a1b2c3d4-e5f6...",
  "max_quota": 500
}
```

### 2. Extract a Video Frame

Provide your video file via form data along with exactly which `minute` and `second` mark it should extract the frame from. The system operates directly on `ffmpeg` commands and responds immediately with binary `image/jpeg` content. 

```bash
curl -X POST http://YOUR-DOMAIN/extract-frame \
  -H "x-api-key: trick-a1b2c3d4-e5f6..." \
  -F "video=@my_meeting_recording.mp4" \
  -F "minute=25" \
  -F "second=40" \
  --output target_frame.jpg
```

## Local Development Requirements

- Cargo / Rust (>=1.80)
- FFmpeg installed in your PATH
- `cargo run` (Launches server natively on `http://127.0.0.1:3000`)
