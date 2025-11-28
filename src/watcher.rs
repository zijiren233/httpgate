use std::sync::Arc;

use futures::StreamExt;
use kube::{
    api::Api,
    config::{KubeConfigOptions, Kubeconfig},
    runtime::{watcher, watcher::Event, WatchStreamExt},
    Client, Config,
};
use tracing::{error, info, warn};

use crate::{crd::Devbox, error::Result, registry::DevboxRegistry};

/// Kubernetes watcher for Devbox resources.
///
/// Watches all Devbox CRDs across all namespaces and maintains
/// a registry of uniqueID -> namespace mappings.
pub struct DevboxWatcher {
    registry: Arc<DevboxRegistry>,
}

impl DevboxWatcher {
    pub fn new(registry: Arc<DevboxRegistry>) -> Self {
        Self { registry }
    }

    /// Create a Kubernetes client.
    ///
    /// Priority:
    /// 1. KUBECONFIG environment variable (if set)
    /// 2. In-cluster config (if running in K8s)
    /// 3. Default kubeconfig
    async fn create_client() -> Result<Client> {
        if let Ok(kubeconfig_path) = std::env::var("KUBECONFIG") {
            info!(path = %kubeconfig_path, "Using KUBECONFIG from environment");
            let kubeconfig = Kubeconfig::read_from(&kubeconfig_path).map_err(|e| {
                crate::error::Error::Config(format!("Failed to read KUBECONFIG: {e}"))
            })?;
            let config = Config::from_custom_kubeconfig(kubeconfig, &KubeConfigOptions::default())
                .await
                .map_err(|e| {
                    crate::error::Error::Config(format!("Failed to parse KUBECONFIG: {e}"))
                })?;
            return Ok(Client::try_from(config)?);
        }

        // Try in-cluster config first, then fall back to default kubeconfig
        if let Ok(config) = Config::incluster() {
            info!("Using in-cluster Kubernetes config");
            Ok(Client::try_from(config)?)
        } else {
            info!("Using default kubeconfig");
            Ok(Client::try_default().await?)
        }
    }

    /// Start watching Devbox resources.
    ///
    /// This function runs indefinitely, processing watch events.
    /// It should be spawned as a background task.
    pub async fn run(&self) -> Result<()> {
        let client = Self::create_client().await?;
        let devboxes: Api<Devbox> = Api::all(client);

        info!("Starting Devbox watcher");

        let watcher_config = watcher::Config::default();
        let mut stream = watcher(devboxes, watcher_config).default_backoff().boxed();

        while let Some(event) = stream.next().await {
            self.handle_event(event);
        }

        warn!("Devbox watcher stream ended unexpectedly");
        Ok(())
    }

    fn handle_event(&self, event: std::result::Result<Event<Devbox>, watcher::Error>) {
        match event {
            // Object was added or modified
            // Object from initial list
            Ok(Event::Apply(devbox) | Event::InitApply(devbox)) => {
                self.handle_apply(&devbox);
            }
            // Object was deleted
            Ok(Event::Delete(devbox)) => {
                self.handle_delete(&devbox);
            }
            // Initial list started - clear registry for fresh sync
            Ok(Event::Init) => {
                info!("Watcher initializing, clearing registry");
                self.registry.clear();
            }
            // Initial list completed
            Ok(Event::InitDone) => {
                info!(
                    count = self.registry.len(),
                    "Watcher initialization complete"
                );
            }
            Err(e) => {
                error!(error = %e, "Watcher error");
            }
        }
    }

    fn handle_apply(&self, devbox: &Devbox) {
        let Some(unique_id) = devbox.unique_id() else {
            warn!(
                namespace = ?devbox.metadata.namespace,
                name = ?devbox.metadata.name,
                "Devbox has no unique_id, skipping"
            );
            return;
        };

        let Some(namespace) = devbox.metadata.namespace.as_ref() else {
            warn!(
                namespace = ?devbox.metadata.namespace,
                name = ?devbox.metadata.name,
                "Devbox has no namespace, skipping"
            );
            return;
        };

        let is_new = self
            .registry
            .register(unique_id.to_string(), namespace.clone());

        if is_new {
            info!(
                unique_id = %unique_id,
                namespace = %namespace,
                "Devbox registered"
            );
        }
    }

    fn handle_delete(&self, devbox: &Devbox) {
        if let Some(unique_id) = devbox.unique_id() {
            self.registry.unregister(unique_id);
        }
    }
}
