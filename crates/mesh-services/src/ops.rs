//! Deploy, undeploy, purge and the `set_*` operations (spec §4.2, §7).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use mesh_core::membership::MESH_NS;
use mesh_core::{Node, NodeHandle};
use mesh_proto::{Limits, Placement, ServiceKind, ServiceManifest, encode_raw, valid_service_name};
use mesh_runtime::{CodeCache, Runtime, RuntimeConfig, RuntimeError};
use tokio::sync::Mutex;

use crate::deployer::{DeployerTask, DeployerTimings, Shared};
use crate::error::ServicesError;
use crate::gc::{GC_EVERY, GcTask, mark_owned};
use crate::manifest::{
    current_generation, data_ns, gen_key, manifest_key, module_too_large, read_manifest,
};

/// Settings for [`Services::start`].
#[derive(Clone, Debug)]
pub struct ServicesConfig {
    pub cache: CodeCache,
    pub max_in_flight: usize,
}

impl Default for ServicesConfig {
    fn default() -> Self {
        let rt = RuntimeConfig::default();
        ServicesConfig {
            cache: rt.cache,
            max_in_flight: rt.max_in_flight,
        }
    }
}

/// What a deploy sets on a new manifest. `Default` is the spec defaults.
#[derive(Clone, Debug)]
pub struct DeployOpts {
    pub limits: Limits,
    pub placement: Placement,
    pub outbound: Vec<String>,
}

impl Default for DeployOpts {
    fn default() -> Self {
        DeployOpts {
            limits: Limits::default(),
            placement: Placement::AllHosts,
            outbound: Vec::new(),
        }
    }
}

/// A node's services: deploy operations, the deployer, dispatch and status.
pub struct Services {
    pub(crate) shared: Arc<Shared>,
    deployer: DeployerTask,
    gc: GcTask,
    /// Serializes this node's manifest read-modify-write cycles.
    write: Mutex<()>,
}

impl Services {
    /// Starts the runtime and the deployer, which stops with the node (or with
    /// [`Services::shutdown`]).
    pub async fn start(node: &Node, cfg: ServicesConfig) -> Result<Services, ServicesError> {
        Self::start_inner(node, cfg, DeployerTimings::default(), GC_EVERY)
    }

    /// [`Services::start`] with other deployer intervals.
    #[cfg(feature = "test-hooks")]
    pub async fn start_with_timings(
        node: &Node,
        cfg: ServicesConfig,
        timings: DeployerTimings,
    ) -> Result<Services, ServicesError> {
        Self::start_inner(node, cfg, timings, GC_EVERY)
    }

    fn start_inner(
        node: &Node,
        cfg: ServicesConfig,
        timings: DeployerTimings,
        gc_every: Duration,
    ) -> Result<Services, ServicesError> {
        let runtime = Runtime::new(RuntimeConfig {
            cache: cfg.cache,
            max_in_flight: cfg.max_in_flight,
        })
        .map_err(ServicesError::Runtime)?;
        let shared = Arc::new(Shared::new(node.handle(), runtime));
        let deployer = DeployerTask::spawn(shared.clone(), timings);
        let gc = GcTask::spawn(shared.clone(), gc_every);
        Ok(Services {
            shared,
            deployer,
            gc,
            write: Mutex::new(()),
        })
    }

    /// Stops the deployer and its fetches, then drops this node's handle, so the node's
    /// store file is released once the node shuts down too.
    pub async fn shutdown(self) {
        let Services {
            shared,
            deployer,
            gc,
            ..
        } = self;
        gc.stop().await;
        deployer.stop().await;
        drop(shared);
    }

    /// While `on`, the deployer leaves the active table as it is (it scans again once
    /// resumed). Fetches already running still complete. Pausing returns once no scan is
    /// running, so none started before the pause can still change the table.
    #[cfg(feature = "test-hooks")]
    pub async fn pause_deployer(&self, on: bool) {
        self.shared.paused.send_replace(on);
        drop(self.shared.scanning.lock().await);
    }

    /// How many scans the deployer has finished.
    #[cfg(feature = "test-hooks")]
    pub fn deployer_scans(&self) -> u64 {
        self.shared.scans.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// [`Services::start`] with another GC interval.
    #[cfg(feature = "test-hooks")]
    pub async fn start_with_gc_interval(
        node: &Node,
        cfg: ServicesConfig,
        every: Duration,
    ) -> Result<Services, ServicesError> {
        Self::start_inner(node, cfg, DeployerTimings::default(), every)
    }

    /// Runs one blob GC pass now.
    #[cfg(any(test, feature = "test-hooks"))]
    pub async fn gc_now(&self) -> Result<(), ServicesError> {
        crate::gc::run_once(&self.shared).await
    }

    /// While `on`, a GC pass that has chosen blobs to remove waits before removing them.
    #[cfg(feature = "test-hooks")]
    pub fn hold_gc(&self, on: bool) {
        self.shared.gc_hold.send_replace(on);
    }

    /// Resolves once a GC pass has reached the wait of [`Services::hold_gc`].
    #[cfg(feature = "test-hooks")]
    pub async fn gc_held(&self) {
        self.shared.gc_reached.notified().await;
    }

    /// The code cache's `(hits, misses)` since start.
    #[cfg(feature = "test-hooks")]
    pub fn cache_stats(&self) -> (u64, u64) {
        self.shared.runtime.cache_stats()
    }

    fn node(&self) -> &NodeHandle {
        &self.shared.node
    }

    pub async fn deploy_wasm(
        &self,
        name: &str,
        wasm: Bytes,
        opts: DeployOpts,
    ) -> Result<ServiceManifest, ServicesError> {
        check_name(name)?;
        validate(&draft(name, ServiceKind::Wasm { module: [0; 32] }, &opts))?;
        if let Some(e) = module_too_large(wasm.len() as u64) {
            return Err(ServicesError::InvalidComponent(e));
        }
        // An invalid component fails here, before anything is stored.
        let hash = *blake3::hash(&wasm).as_bytes();
        self.shared
            .runtime
            .load(hash, &wasm)
            .await
            .map_err(|e| match e {
                RuntimeError::Invalid(m) | RuntimeError::Compile(m) => {
                    ServicesError::InvalidComponent(m)
                }
                e => ServicesError::Runtime(e),
            })?;
        let _deploying = self.shared.gc_lock.read().await;
        let stored = self.node().blobs().add_bytes(wasm).await?;
        mark_owned(&self.shared, stored)?;
        let kind = ServiceKind::Wasm {
            module: *stored.as_bytes(),
        };
        self.write_new(name, kind, opts).await
    }

    pub async fn deploy_static(
        &self,
        name: &str,
        dir: &Path,
        opts: DeployOpts,
    ) -> Result<ServiceManifest, ServicesError> {
        check_name(name)?;
        validate(&draft(name, ServiceKind::Static { root: [0; 32] }, &opts))?;
        if !tokio::fs::metadata(dir).await?.is_dir() {
            return Err(ServicesError::Io(std::io::Error::other(
                "static site root is not a directory",
            )));
        }
        let _deploying = self.shared.gc_lock.read().await;
        let root = self.node().blobs().add_path(dir).await?;
        mark_owned(&self.shared, root)?;
        let kind = ServiceKind::Static {
            root: *root.as_bytes(),
        };
        self.write_new(name, kind, opts).await
    }

    /// Deletes the manifest; the service's data stays. Undeploying a service that is not
    /// deployed is not an error.
    pub async fn undeploy(&self, name: &str) -> Result<(), ServicesError> {
        check_name(name)?;
        let _w = self.write.lock().await;
        if read_manifest(self.node().store(), name).await?.is_some() {
            self.node()
                .store()
                .delete(MESH_NS, &manifest_key(name))
                .await?;
        }
        Ok(())
    }

    /// Starts a new data generation, then tombstones the old namespace on this node.
    /// Allowed whether or not the service is deployed.
    pub async fn purge(&self, name: &str) -> Result<(), ServicesError> {
        check_name(name)?;
        let store = self.node().store();
        let old = current_generation(store, name).await?;
        let new = old.saturating_add(1);
        store
            .set_add(MESH_NS, &gen_key(name), new.to_be_bytes().to_vec())
            .await?;
        store.purge_namespace(&data_ns(name, old)).await?;
        Ok(())
    }

    pub async fn set_placement(
        &self,
        name: &str,
        placement: Placement,
    ) -> Result<ServiceManifest, ServicesError> {
        self.update(name, |m| m.placement = placement).await
    }

    pub async fn set_outbound(
        &self,
        name: &str,
        outbound: Vec<String>,
    ) -> Result<ServiceManifest, ServicesError> {
        self.update(name, |m| m.outbound = outbound).await
    }

    pub async fn set_limits(
        &self,
        name: &str,
        limits: Limits,
    ) -> Result<ServiceManifest, ServicesError> {
        self.update(name, |m| m.limits = limits).await
    }

    /// Rewrites the manifest with `version + 1` after `f`.
    async fn update(
        &self,
        name: &str,
        f: impl FnOnce(&mut ServiceManifest),
    ) -> Result<ServiceManifest, ServicesError> {
        check_name(name)?;
        let _w = self.write.lock().await;
        let mut m = read_manifest(self.node().store(), name)
            .await?
            .ok_or(ServicesError::NotFound)?;
        f(&mut m);
        m.version += 1;
        self.put(m).await
    }

    /// Writes a manifest for `kind` at the next version.
    async fn write_new(
        &self,
        name: &str,
        kind: ServiceKind,
        opts: DeployOpts,
    ) -> Result<ServiceManifest, ServicesError> {
        let _w = self.write.lock().await;
        let version = read_manifest(self.node().store(), name)
            .await?
            .map_or(1, |m| m.version + 1);
        let mut m = draft(name, kind, &opts);
        m.version = version;
        self.put(m).await
    }

    /// Validates, then writes `m`.
    async fn put(&self, m: ServiceManifest) -> Result<ServiceManifest, ServicesError> {
        validate(&m)?;
        self.node()
            .store()
            .set(MESH_NS, &manifest_key(&m.name), encode_raw(&m))
            .await?;
        Ok(m)
    }
}

fn check_name(name: &str) -> Result<(), ServicesError> {
    if valid_service_name(name) {
        Ok(())
    } else {
        Err(ServicesError::InvalidName)
    }
}

fn validate(m: &ServiceManifest) -> Result<(), ServicesError> {
    m.validate()
        .map_err(|e| ServicesError::InvalidManifest(e.to_string()))
}

fn draft(name: &str, kind: ServiceKind, opts: &DeployOpts) -> ServiceManifest {
    ServiceManifest {
        name: name.to_owned(),
        kind,
        version: 1,
        limits: opts.limits,
        placement: opts.placement.clone(),
        outbound: opts.outbound.clone(),
    }
}
