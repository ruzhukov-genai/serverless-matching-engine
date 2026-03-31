// Local dev entrypoint — TCP listener + hyper serve loop.
// Unchanged from original main.rs. Used only for local development.
// Lambda uses src/main.rs (bootstrap binary) with native lambda_http.
use anyhow::Result;
use sme_gateway::build_router;

// Default worker_threads = num_cpus (2 on this machine).
// Tested worker_threads=8: context-switch overhead on 2 vCPUs caused regression.
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .json()
        .init();

    let app = build_router().await?;

    let port = std::env::var("PORT").unwrap_or_else(|_| "3001".to_string());
    let addr = format!("0.0.0.0:{}", port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("listening on http://{} (h2c + TCP_NODELAY)", addr);

    // Use hyper directly for HTTP/1.1 + h2c (cleartext HTTP/2) auto-detection.
    loop {
        let (stream, _remote) = listener.accept().await?;
        // TCP_NODELAY: disable Nagle's algorithm — send small frames immediately.
        // Critical for low-latency JSON responses (typically <4KB).
        let _ = stream.set_nodelay(true);
        let app = app.clone();
        tokio::spawn(async move {
            let hyper_svc = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                let app = app.clone();
                async move {
                    let (parts, body) = req.into_parts();
                    let body = axum::body::Body::new(body);
                    let req = hyper::Request::from_parts(parts, body);
                    let resp = tower_service::Service::call(&mut app.clone(), req).await;
                    resp.map(|r| {
                        let (parts, body) = r.into_parts();
                        hyper::Response::from_parts(parts, body)
                    })
                }
            });
            let builder = hyper_util::server::conn::auto::Builder::new(
                hyper_util::rt::TokioExecutor::new(),
            );
            if let Err(e) = builder.serve_connection_with_upgrades(
                hyper_util::rt::TokioIo::new(stream),
                hyper_svc,
            ).await {
                if !e.to_string().contains("connection reset") {
                    tracing::debug!(error = %e, "connection ended");
                }
            }
        });
    }
}
