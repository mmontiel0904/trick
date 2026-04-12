mod extractor;
mod usage;

use axum::{
    extract::DefaultBodyLimit,
    middleware,
    routing::post,
    Router,
};
use sqlx::sqlite::SqlitePoolOptions;
use std::net::SocketAddr;
use tower_http::{cors::CorsLayer, limit::RequestBodyLimitLayer, trace::TraceLayer, timeout::TimeoutLayer, services::ServeDir};
use std::time::Duration;
use tracing::info;
use utoipa::{
    openapi::security::{ApiKey, ApiKeyValue, SecurityScheme},
    Modify, OpenApi,
};
use utoipa_swagger_ui::SwaggerUi;
use usage::AppState;

#[derive(OpenApi)]
#[openapi(
    paths(extractor::extract_frame, usage::create_key),
    components(schemas(
        extractor::ExtractFrameRequest, 
        usage::CreateKeyRequest, 
        usage::CreateKeyResponse
    )),
    modifiers(&SecurityAddon)
)]
struct ApiDoc;

struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.as_mut().unwrap(); // we can safely unwrap since we have schemas
        components.add_security_scheme(
            "api_key",
            SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::new("x-api-key"))),
        );
        components.add_security_scheme(
            "admin_key",
            SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::new("x-admin-key"))),
        );
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env().add_directive("trick=info".parse()?))
        .init();

    // In Railway, any data you want to persist across restarts MUST be in a volume path.
    // Railway gives a mount path, e.g. /data/db.sqlite. If not provided, fallback to in-memory/local for testing.
    let db_url = std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite:data.db?mode=rwc".to_string());
    
    info!("Connecting to SQLite database at {}", db_url);
    let pool = SqlitePoolOptions::new()
        .max_connections(20)
        .connect(&db_url)
        .await?;

    usage::setup_db(&pool).await?;

    let state = AppState { db_pool: pool };

    // The protected API routes
    let protected_routes = Router::new()
        .route("/extract-frame", post(extractor::extract_frame))
        .layer(middleware::from_fn_with_state(state.clone(), usage::api_key_auth));

    // Admin routes
    let admin_routes = Router::new()
        .route("/admin/keys", post(usage::create_key))
        .with_state(state.clone());

    // The entire app router
    let app = Router::new()
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", ApiDoc::openapi()))
        .merge(protected_routes)
        .merge(admin_routes)
        .fallback_service(ServeDir::new("public"))
        // 500 MB body size limit
        .layer(DefaultBodyLimit::max(500 * 1024 * 1024))
        .layer(RequestBodyLimitLayer::new(500 * 1024 * 1024))
        .layer(CorsLayer::permissive())
        .layer(TimeoutLayer::new(Duration::from_secs(60)))
        .layer(TraceLayer::new_for_http());

    let port = std::env::var("PORT").unwrap_or_else(|_| "3000".to_string());
    let addr: SocketAddr = format!("0.0.0.0:{}", port).parse()?;
    
    info!("Server listening on http://{}", addr);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
