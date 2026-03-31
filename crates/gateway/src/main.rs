// Lambda entrypoint — native lambda_http, no LWA.
// Converts API Gateway HTTP API v2 events directly to axum Request,
// runs the handler, returns the Response. No proxy layer, no TCP listener.
use lambda_http::Error;
use sme_gateway::build_router;

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .json()
        .init();

    let app = build_router().await.map_err(|e| {
        tracing::error!(error = %e, "failed to build router");
        Box::<dyn std::error::Error + Send + Sync>::from(e.to_string())
    })?;

    lambda_http::run(app).await
}
