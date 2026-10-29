use nostr_ssr::config::Config;
use nostr_ssr::engine::Renderer;
use nostr_ssr::mirror::Mirror;
use nostr_ssr::web::{self, App};
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())).init();
    let _ = rustls::crypto::ring::default_provider().install_default();
    let config = Config::load()?;

    let mirror = Mirror::from_config(&config)?;
    if config.relay_port.is_some() {
        mirror.relay.run().await?;
        tracing::info!("relay on {}", mirror.relay.url().await);
    }
    let (m, relays, backfill) = (mirror.clone(), config.relays.clone(), config.backfill.clone());
    tokio::spawn(async move {
        if let Err(e) = m.follow(&relays, &backfill).await {
            tracing::error!("follow failed: {e}");
        }
        let events = m.len().await;
        tracing::info!(events, "initial sync complete");
    });

    let renderer = Renderer::start(config.bundle_path(), config.workers, mirror.clone());
    let bind = config.bind.clone();
    let warm = config.warm.clone();
    let app = App::new(config, mirror, Some(renderer));
    if !warm.is_empty() {
        let app = app.clone();
        tokio::spawn(async move {
            while !app.mirror.priority_loaded() {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            loop {
                web::warm(&app, &warm).await;
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        });
    }
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("listening on {bind}");
    axum::serve(listener, web::router(app)).await?;
    Ok(())
}
