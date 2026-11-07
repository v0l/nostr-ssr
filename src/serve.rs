use crate::config::Config;
use crate::engine::Renderer;
use crate::mirror::Mirror;
use crate::plugin::Plugin;
use std::sync::Arc;
use std::time::Duration;

pub async fn serve(config: Config, plugins: Vec<Arc<dyn Plugin>>) -> anyhow::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let health = config.health.as_ref().map(|h| crate::health::Health::new(h.allow_private));
    let extra = health.iter().map(|h| crate::sweep::filter(h.clone())).collect();
    let mirror = Mirror::from_config_with(&config, extra)?;
    if let (Some(h), Some(settings)) = (health, config.health.clone()) {
        tokio::spawn(crate::sweep::run(mirror.clone(), h, settings.filter, Duration::from_secs(settings.interval_secs)));
    }
    if config.relay_port.is_some() {
        mirror.relay.run().await?;
        tracing::info!("relay on {}", mirror.relay.url().await);
    }
    let (m, relays, backfill, live) = (mirror.clone(), config.relays.clone(), config.backfill.clone(), config.live);
    tokio::spawn(async move {
        if let Err(e) = m.follow_with(&relays, &backfill, live).await {
            tracing::error!("follow failed: {e}");
        }
        let events = m.len().await;
        tracing::info!(events, "initial sync complete");
    });

    let renderer = config.bundle_path().exists().then(|| Renderer::start_with_plugins(config.bundle_path(), config.workers, mirror.clone(), plugins));
    let bind = config.bind.clone();
    let warm = config.warm.clone();
    let app = crate::web::App::new(config, mirror, renderer);
    if !warm.is_empty() {
        let app = app.clone();
        tokio::spawn(async move {
            while !app.mirror.priority_loaded() {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            loop {
                crate::web::warm(&app, &warm).await;
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        });
    }
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("listening on {bind}");
    axum::serve(listener, crate::web::router(app)).await?;
    Ok(())
}
