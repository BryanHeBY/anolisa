use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use tracing::info;

use ws_ckpt_common::backend::{BackendType, StorageBackend};
use ws_ckpt_common::DaemonConfig;

use crate::backends::btrfs_base::{BtrfsBaseBackend, BtrfsBaseScenario};
use crate::backends::btrfs_common;
use crate::backends::btrfs_loop::BtrfsLoopBackend;
use crate::backends::overlayfs::OverlayFsBackend;

/// Result of backend detection, including the backend instance and how it was chosen.
pub struct DetectResult {
    pub backend: Arc<dyn StorageBackend>,
    pub method: String, // "config" or "auto-detect"
}

/// Detect and create the appropriate storage backend based on configuration.
///
/// When `config.backend_type` is "auto", runs the three-level auto-detection:
///   1. Check if the default data path is already on btrfs → BtrfsBase
///   2. Check if any btrfs partition is mounted → BtrfsBase (CrossDisk)
///   3. Fallback → BtrfsLoop (creates a loop device)
///
/// When an explicit backend type is configured, creates that backend directly.
pub(crate) async fn detect_and_create_backend(
    config: &DaemonConfig,
) -> anyhow::Result<DetectResult> {
    match config.parse_backend_type() {
        Some(backend_type) => {
            info!(
                "Backend explicitly configured as '{}', skipping auto-detection",
                config.backend_type
            );
            let backend = create_backend(backend_type, config).await?;
            Ok(DetectResult {
                backend,
                method: "config".to_string(),
            })
        }
        None => {
            // "auto" or unrecognized → run auto-detection
            info!(
                "Backend type is '{}', running auto-detection...",
                config.backend_type
            );
            let backend_type = auto_detect(config).await?;
            info!("Auto-detection selected backend: {}", backend_type);
            let backend = create_backend(backend_type, config).await?;
            Ok(DetectResult {
                backend,
                method: "auto-detect".to_string(),
            })
        }
    }
}

/// Three-level auto-detection logic.
async fn auto_detect(_config: &DaemonConfig) -> anyhow::Result<BackendType> {
    // Level 1: Is the default mount_path parent already on btrfs?
    //   If mount_path (e.g. /mnt/btrfs-workspace) is on btrfs, we can use BtrfsBase InPlace-style.
    //   But actually at daemon startup we don't know workspace paths yet.
    //   We check if there's a btrfs partition available at all.

    // Level 2: Is there an already-mounted btrfs partition?
    if let Ok(mount_info) = btrfs_common::find_available_btrfs_partition().await {
        info!(
            "Auto-detect: found mounted btrfs partition at {} (device: {})",
            mount_info.mount_point, mount_info.device
        );
        return Ok(BackendType::BtrfsBase);
    }

    // Level 3: No btrfs partition found → fallback to BtrfsLoop
    info!("Auto-detect: no mounted btrfs partition found, falling back to BtrfsLoop");
    Ok(BackendType::BtrfsLoop)
}

/// Create a backend instance for the given type.
pub(crate) async fn create_backend(
    backend_type: BackendType,
    config: &DaemonConfig,
) -> anyhow::Result<Arc<dyn StorageBackend>> {
    match backend_type {
        BackendType::BtrfsLoop => {
            let backend = BtrfsLoopBackend::new(
                config.mount_path.clone(),
                PathBuf::from(ws_ckpt_common::BTRFS_IMG_PATH),
            );
            Ok(Arc::new(backend))
        }
        BackendType::BtrfsBase => {
            // Find the best btrfs partition to use
            let mount_info = btrfs_common::find_available_btrfs_partition()
                .await
                .context(
                    "Backend type 'btrfs-base' selected but no mounted btrfs partition found. \
                     Please mount a btrfs partition or switch to 'btrfs-loop' backend.",
                )?;

            // Determine scenario: daemon-level default is CrossDisk.
            // The actual scenario (InPlace vs CrossDisk) is refined per-workspace
            // during init_workspace based on whether the workspace path is on the
            // same btrfs partition.
            let scenario = BtrfsBaseScenario::CrossDisk;
            info!(
                "Creating BtrfsBase backend: mount={}, scenario={:?}",
                mount_info.mount_point, scenario
            );
            let backend = BtrfsBaseBackend::new(PathBuf::from(&mount_info.mount_point), scenario);
            Ok(Arc::new(backend))
        }
        BackendType::OverlayFs => {
            let data_root = PathBuf::from("/data/agent_workspace");
            info!(
                "Creating OverlayFs backend: data_root={}",
                data_root.display()
            );
            let backend = OverlayFsBackend::new(data_root);
            Ok(Arc::new(backend))
        }
    }
}
