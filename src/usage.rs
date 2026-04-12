use axum::{
    body::Body,
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::Response,
};
use serde::{Deserialize, Serialize};
use sqlx::{SqlitePool, Row};
use tracing::{error, info};

#[derive(Clone)]
pub struct AppState {
    pub db_pool: SqlitePool,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CreateKeyRequest {
    pub max_quota: i64,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct CreateKeyResponse {
    pub key: String,
    pub max_quota: i64,
}

pub async fn setup_db(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS api_keys (
            key TEXT PRIMARY KEY,
            usage_count INTEGER DEFAULT 0,
            max_quota INTEGER DEFAULT 100
        );"
    )
    .execute(pool)
    .await?;

    // For demonstration, inset a default test key.
    // In production, you would add an admin endpoint or script for this.
    let default_key = "trick-test-key-123";
    let _ = sqlx::query(
        "INSERT INTO api_keys (key, max_quota) VALUES (?, ?) ON CONFLICT(key) DO NOTHING"
    )
    .bind(default_key)
    .bind(100)
    .execute(pool)
    .await;
    
    info!("Database initialized. Default API key: {}", default_key);

    Ok(())
}

pub async fn api_key_auth(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let api_key = req
        .headers()
        .get("x-api-key")
        .and_then(|h| h.to_str().ok());

    let api_key = match api_key {
        Some(k) => k,
        None => {
            error!("Missing or invalid x-api-key header");
            return Err(StatusCode::UNAUTHORIZED);
        }
    };

    // Atomically increment the usage tally if below quota
    let result = sqlx::query(
        "UPDATE api_keys 
         SET usage_count = usage_count + 1 
         WHERE key = ? AND usage_count < max_quota 
         RETURNING key"
    )
    .bind(api_key)
    .fetch_optional(&state.db_pool)
    .await;

    match result {
        Ok(Some(_)) => {
            // Updated successfully, quota is valid.
            Ok(next.run(req).await)
        }
        Ok(None) => {
            // Two possibilities: key is invalid (doesn't exist) OR quota exceeded.
            // Let's do a simple read to check which is true for better error code.
            let exists = sqlx::query("SELECT usage_count, max_quota FROM api_keys WHERE key = ?")
                .bind(api_key)
                .fetch_optional(&state.db_pool)
                .await
                .map_err(|e| {
                    error!("Database error: {}", e);
                    StatusCode::INTERNAL_SERVER_ERROR
                })?;

            if let Some(row) = exists {
                let usage: i64 = row.try_get("usage_count").unwrap_or(0);
                let quota: i64 = row.try_get("max_quota").unwrap_or(0);
                error!("Quota exceeded for key. Used {}/{}", usage, quota);
                Err(StatusCode::TOO_MANY_REQUESTS)
            } else {
                error!("Invalid API key provided");
                Err(StatusCode::UNAUTHORIZED)
            }
        }
        Err(e) => {
            error!("Database error during auth: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

#[utoipa::path(
    post,
    path = "/admin/keys",
    request_body = CreateKeyRequest,
    responses(
        (status = 200, description = "Key created", body = CreateKeyResponse),
        (status = 401, description = "Unauthorized - Invalid admin key"),
    ),
    security(
        ("admin_key" = [])
    )
)]
pub async fn create_key(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    axum::extract::Json(payload): axum::extract::Json<CreateKeyRequest>,
) -> Result<axum::extract::Json<CreateKeyResponse>, StatusCode> {
    let admin_key = headers.get("x-admin-key").and_then(|v| v.to_str().ok());
    let expected_admin = std::env::var("ADMIN_KEY").unwrap_or_else(|_| "secret-admin".to_string());
    
    if admin_key != Some(&expected_admin) {
        error!("Unauthorized attempt to generate API key");
        return Err(StatusCode::UNAUTHORIZED);
    }

    let new_key = format!("trick-{}", uuid::Uuid::new_v4());
    
    sqlx::query("INSERT INTO api_keys (key, max_quota) VALUES (?, ?)")
        .bind(&new_key)
        .bind(payload.max_quota)
        .execute(&state.db_pool)
        .await
        .map_err(|e| {
            error!("Failed to generate api key: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    info!("New API key generated ending in {} with quota: {}", &new_key[new_key.len()-4..], payload.max_quota);

    Ok(axum::extract::Json(CreateKeyResponse {
        key: new_key,
        max_quota: payload.max_quota,
    }))
}
