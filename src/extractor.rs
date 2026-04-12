use axum::{
    extract::Multipart,
    http::{header, StatusCode},
    response::IntoResponse,
};
use tempfile::Builder;
use tokio::{fs::File, io::AsyncWriteExt, process::Command};
use tracing::{error, info};
use std::process::Stdio;

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
pub async fn extract_frame(mut multipart: Multipart) -> Result<impl IntoResponse, StatusCode> {
    let mut video_temp_file = None;
    let mut minute = None;
    let mut second = None;

    // Process multipart stream
    while let Some(mut field) = multipart.next_field().await.map_err(|e| {
        error!("Error reading multipart: {}", e);
        StatusCode::BAD_REQUEST
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
                    StatusCode::INTERNAL_SERVER_ERROR
                })?
                .into_temp_path();

            let mut file = File::create(&temp_path).await.map_err(|e| {
                error!("Failed to open tempfile for writing: {}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?;

            while let Some(chunk) = field.chunk().await.map_err(|e| {
                error!("Failed reading chunk: {}", e);
                StatusCode::BAD_REQUEST
            })? {
                file.write_all(&chunk).await.map_err(|e| {
                    error!("Failed writing to temp file: {}", e);
                    StatusCode::INTERNAL_SERVER_ERROR
                })?;
            }
            video_temp_file = Some(temp_path);
        } else if name == "minute" {
            let text = field.text().await.map_err(|_| StatusCode::BAD_REQUEST)?;
            minute = Some(text.parse::<u32>().map_err(|_| StatusCode::BAD_REQUEST)?);
        } else if name == "second" {
            let text = field.text().await.map_err(|_| StatusCode::BAD_REQUEST)?;
            second = Some(text.parse::<u32>().map_err(|_| StatusCode::BAD_REQUEST)?);
        }
    }

    let temp_path = video_temp_file.ok_or_else(|| {
        error!("Missing 'video' field in multipart data");
        StatusCode::BAD_REQUEST
    })?;

    let m = minute.unwrap_or(0);
    let s = second.unwrap_or(0);
    let time_str = format!("{:02}:{:02}:{:02}", m / 60, m % 60, s); // Supports minutes > 60

    info!("Extracting frame at time: {} from {:?}", time_str, temp_path.to_str());

    // Call ffmpeg
    // ffmpeg -ss 00:MM:SS -i <input> -frames:v 1 -f image2 -vcodec mjpeg -
    let output = Command::new("ffmpeg")
        .arg("-ss")
        .arg(&time_str)
        .arg("-i")
        .arg(temp_path.as_os_str())
        .arg("-frames:v")
        .arg("1")
        .arg("-f")
        .arg("image2")
        .arg("-vcodec")
        .arg("mjpeg")
        .arg("-") // output to stdout
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| {
            error!("Failed to execute ffmpeg: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    if !output.status.success() {
        let err_msg = String::from_utf8_lossy(&output.stderr);
        error!("FFmpeg command failed: {}", err_msg);
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    // The output.stdout contains our JPEG bytes
    let headers = [
        (header::CONTENT_TYPE, "image/jpeg"),
    ];

    info!("Successfully extracted frame!");

    Ok((headers, output.stdout))
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
