use axum::{
    extract::Multipart,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use tempfile::Builder;
use std::time::Duration;
use tokio::time::timeout;
use tokio::{fs::File, io::AsyncWriteExt, process::Command};
use tracing::{error, info};
use std::process::Stdio;
use serde::Serialize;
use futures::stream::{self, StreamExt};
use zip::write::SimpleFileOptions;

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
    let time_str = format!("{:02}:{:02}:{:02}", m / 60, m % 60, s); // Supports minutes > 60

    info!("Extracting frame at time: {} from {:?}", time_str, temp_path.to_str());

    // Call ffmpeg
    // ffmpeg -ss 00:MM:SS -i <input> -frames:v 1 -f image2 -vcodec mjpeg -
    let ffmpeg_future = Command::new("ffmpeg")
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
        .output();

    let output_result = timeout(Duration::from_secs(30), ffmpeg_future).await;

    let output = match output_result {
        Ok(Ok(res)) => res,
        Ok(Err(e)) => {
            error!("Failed to execute ffmpeg: {}", e);
            tokio::task::spawn_blocking(move || drop(temp_path));
            return Err(StatusCode::INTERNAL_SERVER_ERROR.into_response());
        }
        Err(_) => {
            error!("FFmpeg execution timed out");
            tokio::task::spawn_blocking(move || drop(temp_path));
            return Err((StatusCode::REQUEST_TIMEOUT, "Video processing timed out".to_string()).into_response());
        }
    };

    if !output.status.success() {
        let err_msg = String::from_utf8_lossy(&output.stderr);
        error!("FFmpeg command failed: {}", err_msg);
        tokio::task::spawn_blocking(move || drop(temp_path));
        return Err((StatusCode::BAD_REQUEST, format!("Video extraction failed: {}", err_msg)).into_response());
    }

    // The output.stdout contains our JPEG bytes
    let headers = [
        (header::CONTENT_TYPE, "image/jpeg"),
    ];

    info!("Successfully extracted frame!");

    // Clean up temporary file using standard synchronous thread to unblock Tokio threads
    tokio::task::spawn_blocking(move || {
        drop(temp_path);
    });

    Ok((headers, output.stdout))
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
