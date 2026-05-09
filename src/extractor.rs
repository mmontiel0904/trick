use axum::{
    extract::{rejection::JsonRejection, Multipart},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use tempfile::Builder;
use std::time::Duration;
use tokio::time::timeout;
use tokio::{fs::File, io::AsyncWriteExt, process::Command};
use axum::Json;
use tracing::{error, info};
use reqwest::Client;
use std::process::Stdio;
use serde::Serialize;
use futures::stream::{self, StreamExt};
use zip::write::SimpleFileOptions;

/// Standard error response body returned by all 4xx/5xx responses.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct ErrorResponse {
    /// Human-readable error message.
    pub error: String,
}

/// Convert an Axum JSON rejection (e.g. missing field, bad JSON) into a typed
/// 422 JSON response instead of Axum's default plain-text body.
fn json_rejection_response(rejection: JsonRejection) -> Response {
    let msg = rejection.body_text();
    (StatusCode::UNPROCESSABLE_ENTITY,
     Json(serde_json::json!({"error": msg}))).into_response()
}

async fn run_ffmpeg_extract(
    temp_path: &std::path::Path,
    seek_secs: u64,
) -> Result<Vec<u8>, Response> {
    let time_str = format!("{}", seek_secs);

    info!("run_ffmpeg_extract: seeking to {}s in {:?}", seek_secs, temp_path);

    let ffmpeg_future = Command::new("ffmpeg")
        .arg("-ss").arg(&time_str)
        .arg("-i").arg(temp_path.as_os_str())
        .arg("-frames:v").arg("1")
        .arg("-f").arg("image2")
        .arg("-vcodec").arg("mjpeg")
        .arg("-")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();

    let output_result = timeout(Duration::from_secs(55), ffmpeg_future).await;

    match output_result {
        Ok(Ok(res)) => {
            if res.status.success() {
                Ok(res.stdout)
            } else {
                let err_msg = String::from_utf8_lossy(&res.stderr).to_string();
                error!("FFmpeg failed: {}", err_msg);
                if err_msg.contains("Invalid data") || err_msg.contains("moov atom not found") {
                    Err((StatusCode::UNPROCESSABLE_ENTITY,
                         Json(serde_json::json!({"error": "seek position may exceed video duration"})))
                        .into_response())
                } else {
                    Err((StatusCode::INTERNAL_SERVER_ERROR,
                         Json(serde_json::json!({"error": format!("ffmpeg failed: {}", err_msg)})))
                        .into_response())
                }
            }
        }
        Ok(Err(e)) => {
            error!("Failed to execute ffmpeg: {}", e);
            Err((StatusCode::INTERNAL_SERVER_ERROR,
                 Json(serde_json::json!({"error": "ffmpeg execution failed"})))
                .into_response())
        }
        Err(_) => {
            error!("FFmpeg timed out");
            Err((StatusCode::INTERNAL_SERVER_ERROR,
                 Json(serde_json::json!({"error": "video processing timed out"})))
                .into_response())
        }
    }
}

/// Extract a frame from a video segment
///
/// This endpoint accepts a `multipart/form-data` request containing a video file,
/// and the time coordinates (`minute` and `second`) to extract a single frame.
/// 
/// Returns `image/jpeg` binary data on success.
#[utoipa::path(
    post,
    path = "/extract-frame",
    request_body(
        content = inline(ExtractFrameRequest),
        content_type = "multipart/form-data",
        description = "Multipart payload containing the video and timestamp."
    ),
    responses(
        (status = 200, description = "The extracted JPEG frame", content_type = "image/jpeg"),
        (status = 400, description = "Bad Request"),
        (status = 401, description = "Unauthorized - Missing or invalid API Key"),
        (status = 429, description = "Too Many Requests - Quota exceeded"),
        (status = 500, description = "Internal Server Error")
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn extract_frame(mut multipart: Multipart) -> Result<impl IntoResponse, Response> {
    let mut video_temp_file = None;
    let mut minute = None;
    let mut second = None;

    // Process multipart stream
    while let Some(mut field) = multipart.next_field().await.map_err(|e| {
        error!("Error reading multipart: {}", e);
        StatusCode::BAD_REQUEST.into_response()
    })? {
        let name = field.name().unwrap_or("").to_string();

        if name == "video" {
            // Write the video securely to a temp file, chunk by chunk.
            let temp_path = Builder::new()
                .prefix("video-upload-")
                .suffix(".tmp")
                .tempfile()
                .map_err(|e| {
                    error!("Failed to create tempfile: {}", e);
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                })?
                .into_temp_path();

            let mut file = File::create(&temp_path).await.map_err(|e| {
                error!("Failed to open tempfile for writing: {}", e);
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            })?;

            while let Some(chunk) = field.chunk().await.map_err(|e| {
                error!("Failed reading chunk: {}", e);
                StatusCode::BAD_REQUEST.into_response()
            })? {
                file.write_all(&chunk).await.map_err(|e| {
                    error!("Failed writing to temp file: {}", e);
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                })?;
            }
            video_temp_file = Some(temp_path);
        } else if name == "minute" {
            let text = field.text().await.map_err(|_| StatusCode::BAD_REQUEST.into_response())?;
            minute = Some(text.parse::<u32>().map_err(|_| StatusCode::BAD_REQUEST.into_response())?);
        } else if name == "second" {
            let text = field.text().await.map_err(|_| StatusCode::BAD_REQUEST.into_response())?;
            second = Some(text.parse::<u32>().map_err(|_| StatusCode::BAD_REQUEST.into_response())?);
        }
    }

    let temp_path = video_temp_file.ok_or_else(|| {
        error!("Missing 'video' field in multipart data");
        StatusCode::BAD_REQUEST.into_response()
    })?;

    let m = minute.unwrap_or(0);
    let s = second.unwrap_or(0);
    let seek_secs = (m as u64) * 60 + (s as u64);

    let jpeg_bytes = run_ffmpeg_extract(&temp_path, seek_secs).await?;

    tokio::task::spawn_blocking(move || drop(temp_path));

    info!("extract_frame: frame extracted successfully");

    let headers = [(header::CONTENT_TYPE, "image/jpeg")];
    Ok((headers, jpeg_bytes))
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct ExtractionReport {
    pub successful_timestamps: Vec<String>,
    pub failed_timestamps: Vec<FailedTimestamp>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct FailedTimestamp {
    pub timestamp: String,
    pub reason: String,
}

#[derive(utoipa::ToSchema)]
#[allow(dead_code)]
pub struct ExtractFramesRequest {
    /// The video file to process
    #[schema(value_type = String, format = Binary)]
    video: String,
    
    /// Comma-separated list of formatted timestamps (e.g., '10,90,01:30')
    timestamps: String,
}

/// Extract multiple frames from a single video segment
///
/// This endpoint accepts a `multipart/form-data` request containing a video file,
/// and a comma-separated list of timestamps (`timestamps`).
/// 
/// Returns an `application/zip` containing the extracted JPEGs and an `extraction_report.json` describing successes and failures.
#[utoipa::path(
    post,
    path = "/extract-frames",
    request_body(
        content = inline(ExtractFramesRequest),
        content_type = "multipart/form-data",
        description = "Multipart payload containing the video and multiple timestamps."
    ),
    responses(
        (status = 200, description = "A zip file containing the frames and an extraction_report.json log", content_type = "application/zip"),
        (status = 400, description = "Bad Request"),
        (status = 401, description = "Unauthorized - Missing or invalid API Key"),
        (status = 429, description = "Too Many Requests - Quota exceeded"),
        (status = 500, description = "Internal Server Error")
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn extract_frames(mut multipart: Multipart) -> Result<impl IntoResponse, Response> {
    let mut video_temp_file = None;
    let mut timestamps_string = None;

    // Process multipart stream
    while let Some(mut field) = multipart.next_field().await.map_err(|e| {
        error!("Error reading multipart: {}", e);
        StatusCode::BAD_REQUEST.into_response()
    })? {
        let name = field.name().unwrap_or("").to_string();

        if name == "video" {
            let temp_path = Builder::new()
                .prefix("video-upload-")
                .suffix(".tmp")
                .tempfile()
                .map_err(|e| {
                    error!("Failed to create tempfile: {}", e);
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                })?
                .into_temp_path();

            let mut file = File::create(&temp_path).await.map_err(|e| {
                error!("Failed to open tempfile for writing: {}", e);
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            })?;

            while let Some(chunk) = field.chunk().await.map_err(|e| {
                error!("Failed reading chunk: {}", e);
                StatusCode::BAD_REQUEST.into_response()
            })? {
                file.write_all(&chunk).await.map_err(|e| {
                    error!("Failed writing to temp file: {}", e);
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                })?;
            }
            video_temp_file = Some(temp_path);
        } else if name == "timestamps" {
            let text = field.text().await.map_err(|_| StatusCode::BAD_REQUEST.into_response())?;
            timestamps_string = Some(text);
        }
    }

    let temp_path = video_temp_file.ok_or_else(|| {
        error!("Missing 'video' field in multipart data");
        StatusCode::BAD_REQUEST.into_response()
    })?;

    let timestamps_raw = match timestamps_string {
        Some(s) => s,
        None => {
            error!("Missing 'timestamps' field in multipart data");
            tokio::task::spawn_blocking(move || drop(temp_path));
            return Err(StatusCode::BAD_REQUEST.into_response());
        }
    };

    // Parse timestamps
    let timestamps_list: Vec<String> = timestamps_raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if timestamps_list.is_empty() {
        tokio::task::spawn_blocking(move || drop(temp_path));
        return Err((StatusCode::BAD_REQUEST, "No timestamps provided".to_string()).into_response());
    }

    if timestamps_list.len() > 60 {
        tokio::task::spawn_blocking(move || drop(temp_path));
        return Err((StatusCode::BAD_REQUEST, "Exceeded maximum of 60 frames per request".to_string()).into_response());
    }

    let temp_path_arc = std::sync::Arc::new(temp_path);

    // Concurrency processing: Limit to 8 concurrent FFmpeg instances to save RAM
    let mut successes = Vec::new();
    let mut failures = Vec::new();

    let stream_arc = temp_path_arc.clone();
    let mut stream = stream::iter(timestamps_list).map(move |time_str| {
        let temp_path_ref = std::sync::Arc::clone(&stream_arc);
        async move {
            let ts_original = time_str.clone();
            
            // Format time string to allow pure seconds or MM:SS or HH:MM:SS
            let formatted_time = if ts_original.contains(':') {
                ts_original.clone()
            } else if let Ok(seconds) = ts_original.parse::<u32>() {
                format!("{:02}:{:02}:{:02}", seconds / 3600, (seconds % 3600) / 60, seconds % 60)
            } else {
                return (ts_original, Err("Invalid timestamp format, must be seconds or MM:SS".to_string()));
            };

            let ffmpeg_future = Command::new("ffmpeg")
                .arg("-ss")
                .arg(&formatted_time)
                .arg("-i")
                .arg(temp_path_ref.as_os_str())
                .arg("-frames:v")
                .arg("1")
                .arg("-f")
                .arg("image2")
                .arg("-vcodec")
                .arg("mjpeg")
                .arg("-") // output to stdout
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output();

            let output_result = timeout(Duration::from_secs(30), ffmpeg_future).await;

            match output_result {
                Ok(Ok(res)) => {
                    if res.status.success() {
                        let mut safe_filename = ts_original.replace(":", "-");
                        safe_filename = safe_filename.replace("/", "_");
                        (ts_original, Ok((safe_filename, res.stdout)))
                    } else {
                        let err_msg = String::from_utf8_lossy(&res.stderr);
                        (ts_original, Err(format!("FFmpeg failed: {}", err_msg)))
                    }
                }
                Ok(Err(e)) => (ts_original, Err(format!("FFmpeg execution failed: {}", e))),
                Err(_) => (ts_original, Err("FFmpeg execution timed out".to_string())),
            }
        }
    }).buffer_unordered(8); 

    while let Some((ts, result)) = stream.next().await {
        match result {
            Ok((_, bytes)) => {
                let mut safe_filename = ts.replace(":", "-");
                safe_filename = safe_filename.replace("/", "_");
                successes.push((safe_filename, bytes));
            },
            Err(reason) => failures.push(FailedTimestamp { timestamp: ts, reason }),
        }
    }

    // Assemble Zip
    let zip_bytes_result = tokio::task::spawn_blocking(move || {
        use std::io::Write;
        
        let mut report = ExtractionReport {
            successful_timestamps: Vec::new(),
            failed_timestamps: failures,
        };

        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            
            for (filename, bytes) in successes {
                let options = SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored); // store directly, JPEGs already compressed
                
                if zip.start_file(format!("frame_{}.jpg", filename), options).is_ok() {
                    let _ = zip.write_all(&bytes);
                    report.successful_timestamps.push(filename);
                }
            }

            let options = SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            if zip.start_file("extraction_report.json", options).is_ok() {
                let json_bytes = serde_json::to_vec_pretty(&report).unwrap_or_default();
                let _ = zip.write_all(&json_bytes);
            }
            
            zip.finish().unwrap();
        }
        buf.into_inner()
    }).await;

    let zip_bytes = match zip_bytes_result {
        Ok(b) => b,
        Err(e) => {
            error!("Failed to run zip generation task: {}", e);
            tokio::task::spawn_blocking(move || {
                let _arc = temp_path_arc;
            });
            return Err((StatusCode::INTERNAL_SERVER_ERROR, "Failed to build zip archive").into_response());
        }
    };

    // Cleanup temp path arc
    tokio::task::spawn_blocking(move || {
        let _arc = temp_path_arc;
    });

    let headers = [
        (header::CONTENT_TYPE, "application/zip"),
        (header::CONTENT_DISPOSITION, "attachment; filename=\"extracted_frames.zip\"")
    ];

    info!("Successfully extracted multiple frames and assembled zip!");

    Ok((headers, zip_bytes))
}

// Dummy struct just for OpenAPI generation describing the multipart fields.
#[derive(utoipa::ToSchema)]
#[allow(dead_code)]
pub struct ExtractFrameRequest {
    /// The video file to process
    #[schema(value_type = String, format = Binary)]
    video: String,
    
    /// The minute of the timestamp
    minute: u32,
    
    /// The second of the timestamp
    second: u32,
}

/// JSON body for POST /extract-frame-url
#[derive(serde::Deserialize, utoipa::ToSchema)]
pub struct ExtractFrameUrlRequest {
    /// Google Drive download URL.
    /// Format: https://www.googleapis.com/drive/v3/files/{fileId}?alt=media
    pub video_url: String,

    /// Short-lived Google OAuth2 bearer token (from ScriptApp.getOAuthToken()).
    /// Passed as Authorization: Bearer {token} when fetching from Drive.
    pub access_token: String,

    /// Minutes component of seek position (total minutes, e.g. 65 for 1h05m)
    pub minute: u32,

    /// Seconds component of seek position (0–59)
    pub second: u32,
}

const MAX_VIDEO_BYTES: u64 = 2 * 1024 * 1024 * 1024; // 2 GB

/// Extract a single frame from a Google Drive video URL
///
/// Accepts a JSON body with a Drive download URL and short-lived OAuth token.
/// The server fetches the video directly from Drive — the caller never holds
/// video bytes in memory. Returns a raw JPEG frame at the requested timestamp.
#[utoipa::path(
    post,
    path = "/extract-frame-url",
    request_body(
        content = ExtractFrameUrlRequest,
        content_type = "application/json",
        description = "Drive URL, OAuth token, and timestamp."
    ),
    responses(
        (status = 200, description = "The extracted JPEG frame", content_type = "image/jpeg",
         body = Vec<u8>),
        (status = 400, description = "Bad Request — missing or invalid fields",
         body = ErrorResponse, content_type = "application/json"),
        (status = 401, description = "Unauthorized — Drive rejected the token",
         body = ErrorResponse, content_type = "application/json"),
        (status = 403, description = "Forbidden — caller lacks permission to the file",
         body = ErrorResponse, content_type = "application/json"),
        (status = 404, description = "Not Found — bad fileId or file deleted",
         body = ErrorResponse, content_type = "application/json"),
        (status = 413, description = "Payload Too Large — video exceeds 2 GB limit",
         body = ErrorResponse, content_type = "application/json"),
        (status = 422, description = "Unprocessable — seek position beyond video duration or bad JSON",
         body = ErrorResponse, content_type = "application/json"),
        (status = 500, description = "Internal Server Error",
         body = ErrorResponse, content_type = "application/json")
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn extract_frame_url(
    body: Result<Json<ExtractFrameUrlRequest>, JsonRejection>,
) -> Result<impl IntoResponse, Response> {
    let Json(body) = body.map_err(json_rejection_response)?;

    // 1. Validate
    if body.video_url.is_empty() {
        return Err((StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": "video_url is required"}))).into_response());
    }
    if body.access_token.is_empty() {
        return Err((StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": "access_token is required"}))).into_response());
    }
    if body.second > 59 {
        return Err((StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": "second must be 0-59"}))).into_response());
    }

    let seek_secs = body.minute as u64 * 60 + body.second as u64;

    // 2. Fetch video from Drive
    let client = Client::builder()
        .use_rustls_tls()
        .build()
        .map_err(|e| {
            error!("Failed to build HTTP client: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR,
             Json(serde_json::json!({"error": "internal error"}))).into_response()
        })?;

    let resp = client
        .get(&body.video_url)
        .header("Authorization", format!("Bearer {}", body.access_token))
        .send()
        .await
        .map_err(|e| {
            error!("Drive fetch failed: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR,
             Json(serde_json::json!({"error": format!("Drive fetch failed: {}", e)}))).into_response()
        })?;

    // 3. Map Drive status codes
    match resp.status().as_u16() {
        200 => {},
        401 => return Err((StatusCode::UNAUTHORIZED,
                           Json(serde_json::json!({"error": "Drive returned 401: expired or invalid token"}))).into_response()),
        403 => return Err((StatusCode::FORBIDDEN,
                           Json(serde_json::json!({"error": "Drive returned 403: no permission for this file"}))).into_response()),
        404 => return Err((StatusCode::NOT_FOUND,
                           Json(serde_json::json!({"error": "Drive returned 404: file not found"}))).into_response()),
        s   => return Err((StatusCode::INTERNAL_SERVER_ERROR,
                           Json(serde_json::json!({"error": format!("Drive returned unexpected status {}", s)}))).into_response()),
    }

    // 4. Check size before streaming (when Content-Length is present)
    if let Some(content_length) = resp.content_length() {
        if content_length > MAX_VIDEO_BYTES {
            return Err((StatusCode::PAYLOAD_TOO_LARGE,
                        Json(serde_json::json!({"error": "video exceeds 2 GB server limit"}))).into_response());
        }
    }

    // 5. Stream to temp file
    let temp_file = Builder::new()
        .prefix("drive-video-")
        .suffix(".tmp")
        .tempfile()
        .map_err(|e| {
            error!("Failed to create tempfile: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR,
             Json(serde_json::json!({"error": "internal error"}))).into_response()
        })?;
    let temp_path = temp_file.into_temp_path();

    {
        let mut file = File::create(&temp_path).await.map_err(|e| {
            error!("Failed to open tempfile: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR,
             Json(serde_json::json!({"error": "internal error"}))).into_response()
        })?;

        let mut stream = resp.bytes_stream();
        let mut total: u64 = 0;
        use futures::StreamExt as _;
        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.map_err(|e| {
                error!("Drive stream read error: {}", e);
                (StatusCode::INTERNAL_SERVER_ERROR,
                 Json(serde_json::json!({"error": "stream read error"}))).into_response()
            })?;
            total += chunk.len() as u64;
            if total > MAX_VIDEO_BYTES {
                return Err((StatusCode::PAYLOAD_TOO_LARGE,
                            Json(serde_json::json!({"error": "video exceeds 2 GB server limit"}))).into_response());
            }
            file.write_all(&chunk).await.map_err(|e| {
                error!("Failed to write chunk: {}", e);
                (StatusCode::INTERNAL_SERVER_ERROR,
                 Json(serde_json::json!({"error": "internal error"}))).into_response()
            })?;
        }
    } // file flushed/closed here

    // 6. Extract frame using shared helper (same logic as extract_frame)
    let jpeg_bytes = run_ffmpeg_extract(&temp_path, seek_secs).await?;

    info!("extract_frame_url: frame extracted successfully");

    let headers = [(header::CONTENT_TYPE, "image/jpeg")];
    Ok((headers, jpeg_bytes))
}

/// JSON body for POST /extract-frames-url
#[derive(serde::Deserialize, utoipa::ToSchema)]
pub struct ExtractFramesUrlRequest {
    /// Google Drive download URL.
    /// Format: https://www.googleapis.com/drive/v3/files/{fileId}?alt=media
    pub video_url: String,

    /// Short-lived Google OAuth2 bearer token (from ScriptApp.getOAuthToken()).
    /// Passed as Authorization: Bearer {token} when fetching from Drive.
    pub access_token: String,

    /// List of timestamps to extract. Supports formats: plain seconds ("90"),
    /// MM:SS ("1:30"), or HH:MM:SS ("01:05:30"). Max 60 entries.
    pub timestamps: Vec<String>,
}

/// Extract multiple frames from a Google Drive video URL
///
/// Accepts a JSON body with a Drive download URL, short-lived OAuth token,
/// and a list of timestamps. The server fetches the video directly from Drive
/// and extracts all frames concurrently. Returns a ZIP containing the JPEGs
/// and an extraction_report.json describing successes and failures.
#[utoipa::path(
    post,
    path = "/extract-frames-url",
    request_body(
        content = ExtractFramesUrlRequest,
        content_type = "application/json",
        description = "Drive URL, OAuth token, and list of timestamps."
    ),
    responses(
        (status = 200, description = "ZIP archive with JPEG frames and extraction_report.json",
         content_type = "application/zip", body = Vec<u8>),
        (status = 400, description = "Bad Request — missing or invalid fields",
         body = ErrorResponse, content_type = "application/json"),
        (status = 401, description = "Unauthorized — Drive rejected the token",
         body = ErrorResponse, content_type = "application/json"),
        (status = 403, description = "Forbidden — caller lacks permission to the file",
         body = ErrorResponse, content_type = "application/json"),
        (status = 404, description = "Not Found — bad fileId or file deleted",
         body = ErrorResponse, content_type = "application/json"),
        (status = 413, description = "Payload Too Large — video exceeds 2 GB limit",
         body = ErrorResponse, content_type = "application/json"),
        (status = 422, description = "Unprocessable — bad JSON or too many timestamps",
         body = ErrorResponse, content_type = "application/json"),
        (status = 500, description = "Internal Server Error",
         body = ErrorResponse, content_type = "application/json")
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn extract_frames_url(
    body: Result<Json<ExtractFramesUrlRequest>, JsonRejection>,
) -> Result<impl IntoResponse, Response> {
    let Json(body) = body.map_err(json_rejection_response)?;

    // 1. Validate
    if body.video_url.is_empty() {
        return Err((StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": "video_url is required"}))).into_response());
    }
    if body.access_token.is_empty() {
        return Err((StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": "access_token is required"}))).into_response());
    }
    if body.timestamps.is_empty() {
        return Err((StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": "timestamps array is required and must not be empty"}))).into_response());
    }
    if body.timestamps.len() > 60 {
        return Err((StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": "exceeded maximum of 60 timestamps per request"}))).into_response());
    }

    // 2. Fetch video from Drive
    let client = Client::builder()
        .use_rustls_tls()
        .build()
        .map_err(|e| {
            error!("Failed to build HTTP client: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR,
             Json(serde_json::json!({"error": "internal error"}))).into_response()
        })?;

    let resp = client
        .get(&body.video_url)
        .header("Authorization", format!("Bearer {}", body.access_token))
        .send()
        .await
        .map_err(|e| {
            error!("Drive fetch failed: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR,
             Json(serde_json::json!({"error": format!("Drive fetch failed: {}", e)}))).into_response()
        })?;

    // 3. Map Drive status codes
    match resp.status().as_u16() {
        200 => {},
        401 => return Err((StatusCode::UNAUTHORIZED,
                           Json(serde_json::json!({"error": "Drive returned 401: expired or invalid token"}))).into_response()),
        403 => return Err((StatusCode::FORBIDDEN,
                           Json(serde_json::json!({"error": "Drive returned 403: no permission for this file"}))).into_response()),
        404 => return Err((StatusCode::NOT_FOUND,
                           Json(serde_json::json!({"error": "Drive returned 404: file not found"}))).into_response()),
        s   => return Err((StatusCode::INTERNAL_SERVER_ERROR,
                           Json(serde_json::json!({"error": format!("Drive returned unexpected status {}", s)}))).into_response()),
    }

    // 4. Size guard
    if let Some(content_length) = resp.content_length() {
        if content_length > MAX_VIDEO_BYTES {
            return Err((StatusCode::PAYLOAD_TOO_LARGE,
                        Json(serde_json::json!({"error": "video exceeds 2 GB server limit"}))).into_response());
        }
    }

    // 5. Stream to temp file
    let temp_file = Builder::new()
        .prefix("drive-video-batch-")
        .suffix(".tmp")
        .tempfile()
        .map_err(|e| {
            error!("Failed to create tempfile: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR,
             Json(serde_json::json!({"error": "internal error"}))).into_response()
        })?;
    let temp_path = temp_file.into_temp_path();

    {
        let mut file = File::create(&temp_path).await.map_err(|e| {
            error!("Failed to open tempfile: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR,
             Json(serde_json::json!({"error": "internal error"}))).into_response()
        })?;

        let mut stream = resp.bytes_stream();
        let mut total: u64 = 0;
        use futures::StreamExt as _;
        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.map_err(|e| {
                error!("Drive stream read error: {}", e);
                (StatusCode::INTERNAL_SERVER_ERROR,
                 Json(serde_json::json!({"error": "stream read error"}))).into_response()
            })?;
            total += chunk.len() as u64;
            if total > MAX_VIDEO_BYTES {
                return Err((StatusCode::PAYLOAD_TOO_LARGE,
                            Json(serde_json::json!({"error": "video exceeds 2 GB server limit"}))).into_response());
            }
            file.write_all(&chunk).await.map_err(|e| {
                error!("Failed to write chunk: {}", e);
                (StatusCode::INTERNAL_SERVER_ERROR,
                 Json(serde_json::json!({"error": "internal error"}))).into_response()
            })?;
        }
    } // file flushed/closed here

    // 6. Concurrent ffmpeg extraction (same logic as extract_frames)
    let temp_path_arc = std::sync::Arc::new(temp_path);
    let mut successes: Vec<(String, Vec<u8>)> = Vec::new();
    let mut failures: Vec<FailedTimestamp> = Vec::new();

    let stream_arc = temp_path_arc.clone();
    let mut stream = stream::iter(body.timestamps).map(move |time_str| {
        let temp_path_ref = std::sync::Arc::clone(&stream_arc);
        async move {
            let ts_original = time_str.clone();

            let formatted_time = if ts_original.contains(':') {
                ts_original.clone()
            } else if let Ok(seconds) = ts_original.parse::<u32>() {
                format!("{:02}:{:02}:{:02}", seconds / 3600, (seconds % 3600) / 60, seconds % 60)
            } else {
                return (ts_original, Err("Invalid timestamp format, must be seconds or MM:SS or HH:MM:SS".to_string()));
            };

            let ffmpeg_future = Command::new("ffmpeg")
                .arg("-ss")
                .arg(&formatted_time)
                .arg("-i")
                .arg(temp_path_ref.as_os_str())
                .arg("-frames:v")
                .arg("1")
                .arg("-f")
                .arg("image2")
                .arg("-vcodec")
                .arg("mjpeg")
                .arg("-")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output();

            let output_result = timeout(Duration::from_secs(30), ffmpeg_future).await;

            match output_result {
                Ok(Ok(res)) => {
                    if res.status.success() {
                        let safe_filename = ts_original.replace(':', "-").replace('/', "_");
                        (ts_original, Ok((safe_filename, res.stdout)))
                    } else {
                        let err_msg = String::from_utf8_lossy(&res.stderr);
                        (ts_original, Err(format!("FFmpeg failed: {}", err_msg)))
                    }
                }
                Ok(Err(e)) => (ts_original, Err(format!("FFmpeg execution failed: {}", e))),
                Err(_) => (ts_original, Err("FFmpeg execution timed out".to_string())),
            }
        }
    }).buffer_unordered(8);

    while let Some((ts, result)) = stream.next().await {
        match result {
            Ok((safe_filename, bytes)) => successes.push((safe_filename, bytes)),
            Err(reason) => failures.push(FailedTimestamp { timestamp: ts, reason }),
        }
    }

    // 7. Assemble ZIP (same as extract_frames)
    let zip_bytes_result = tokio::task::spawn_blocking(move || {
        use std::io::Write;

        let mut report = ExtractionReport {
            successful_timestamps: Vec::new(),
            failed_timestamps: failures,
        };
        report.failed_timestamps.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

        let mut zip_buf = std::io::Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut zip_buf);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);

            for (filename, jpeg_bytes) in &successes {
                zip.start_file(format!("{}.jpg", filename), options)
                   .map_err(|e| format!("ZIP write error: {}", e))?;
                zip.write_all(jpeg_bytes)
                   .map_err(|e| format!("ZIP data write error: {}", e))?;
                report.successful_timestamps.push(filename.clone());
            }

            let report_json = serde_json::to_string_pretty(&report)
                .map_err(|e| format!("Report serialization error: {}", e))?;
            zip.start_file("extraction_report.json", options)
               .map_err(|e| format!("ZIP report error: {}", e))?;
            zip.write_all(report_json.as_bytes())
               .map_err(|e| format!("ZIP report write error: {}", e))?;

            zip.finish().map_err(|e| format!("ZIP finish error: {}", e))?;
        }

        Ok::<Vec<u8>, String>(zip_buf.into_inner())
    }).await;

    let zip_bytes = zip_bytes_result
        .map_err(|e| {
            error!("spawn_blocking panicked: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR,
             Json(serde_json::json!({"error": "internal error"}))).into_response()
        })?
        .map_err(|e| {
            error!("ZIP assembly failed: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR,
             Json(serde_json::json!({"error": e}))).into_response()
        })?;

    info!("extract_frames_url: ZIP assembled successfully");

    let headers = [(header::CONTENT_TYPE, "application/zip")];
    Ok((headers, zip_bytes))
}
