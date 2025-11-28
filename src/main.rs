use std::{sync::Arc, time::Duration};

use pingora_core::server::{configuration::Opt, Server};
use tracing::{error, info};

use httpgate::{
    config::Config, proxy::DevboxProxy, registry::DevboxRegistry, watcher::DevboxWatcher,
};

fn init_logging(log_level: &str) {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(format!("httpgate={log_level}").parse().unwrap())
                .add_directive("pingora=warn".parse().unwrap()),
        )
        .init();
}

fn main() {
    // Load configuration
    let config = Config::from_env();

    // Initialize logging
    init_logging(&config.log_level);

    info!(
        listen_addr = %config.listen_addr,
        domain_suffix = %config.domain_suffix,
        "Starting httpgate"
    );

    // Create shared registry
    let registry = Arc::new(DevboxRegistry::new());

    // Create Pingora server
    let opt = Opt::default();
    let mut server = Server::new(Some(opt)).unwrap();
    server.bootstrap();

    // Create and configure proxy service
    let proxy = DevboxProxy::new(Arc::clone(&registry), config.domain_suffix);
    let mut proxy_service = pingora_proxy::http_proxy_service(&server.configuration, proxy);
    proxy_service.add_tcp(&config.listen_addr.to_string());

    server.add_service(proxy_service);

    // Spawn Kubernetes watcher in background
    let watcher_registry = Arc::clone(&registry);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to create Tokio runtime");

    runtime.spawn(async move {
        let watcher = DevboxWatcher::new(watcher_registry);
        loop {
            if let Err(e) = watcher.run().await {
                error!(error = %e, "Watcher failed, restarting in 5s");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    });

    info!("Proxy server starting");

    // Run server (blocking)
    server.run_forever();
}
