use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let config = nostr_ssr::config::Config::load()?;
    let plugins: Vec<Arc<dyn nostr_ssr::plugin::Plugin>> = vec![];
    nostr_ssr::serve::serve(config, plugins).await
}
