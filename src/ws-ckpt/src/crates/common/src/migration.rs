//! Legacy index migration: move workspace index.json files from the old
//! in-snapshot layout to state_dir/indexes/ and generate state.json.

use std::fs;
use std::path::Path;

use anyhow::Context;
use tracing::{info, warn};

use crate::backend::StorageBackend;
use crate::persist::{
    self, BackendIdentity, BackendPaths, DaemonStateFile, WorkspaceEntry, DAEMON_STATE_VERSION,
};
use crate::{SnapshotIndex, INDEXES_DIR, INDEX_FILE};

/// Atomically write a `SnapshotIndex` to `ws_dir/index.json` via tmp+rename.
fn save_index_sync(ws_dir: &Path, index: &SnapshotIndex) -> anyhow::Result<()> {
    let index_path = ws_dir.join(INDEX_FILE);
    let tmp_path = ws_dir.join(format!("{}.tmp", INDEX_FILE));
    let content =
        serde_json::to_string_pretty(index).context("Failed to serialize SnapshotIndex")?;
    std::fs::write(&tmp_path, &content)
        .with_context(|| format!("Failed to write {:?}", tmp_path))?;
    std::fs::rename(&tmp_path, &index_path)
        .with_context(|| format!("Failed to rename {:?} -> {:?}", tmp_path, index_path))?;
    Ok(())
}

/// Migrate old position index.json files to the new state_dir/indexes/ directory.
///
/// When state.json does not exist (upgrade scenario), scan old position and migrate.
/// Returns true if a migration occurred.
pub fn migrate_legacy_indexes(backend: &dyn StorageBackend, state_dir: &Path) -> bool {
    let snapshots_root = backend.snapshots_root().to_path_buf();

    let read_dir = match fs::read_dir(&snapshots_root) {
        Ok(rd) => rd,
        Err(_) => return false,
    };

    let mut migrated_any = false;
    let mut logged_migration_start = false;
    let mut workspace_entries: Vec<WorkspaceEntry> = Vec::new();
    let new_indexes_root = state_dir.join(INDEXES_DIR);

    for entry in read_dir {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if !file_type.is_dir() {
            continue;
        }

        let ws_id = match path.file_name() {
            Some(name) => name.to_string_lossy().to_string(),
            None => continue,
        };

        let old_index_path = path.join(INDEX_FILE);
        if !old_index_path.exists() {
            continue;
        }

        // Log migration start on first workspace that needs migration
        if !logged_migration_start {
            info!(
                "Migrating from v0 layout: moving indexes from {:?} to {:?}",
                snapshots_root, new_indexes_root
            );
            logged_migration_start = true;
        }

        // Read old index
        let content = match fs::read_to_string(&old_index_path) {
            Ok(c) => c,
            Err(e) => {
                warn!(
                    "Migration: failed to read old index file {:?}: {}",
                    old_index_path, e
                );
                continue;
            }
        };

        let index: SnapshotIndex = match serde_json::from_str(&content) {
            Ok(idx) => idx,
            Err(e) => {
                warn!(
                    "Migration: failed to parse old index file {:?}: {}",
                    old_index_path, e
                );
                continue;
            }
        };

        // Create new position directory and write it
        let new_index_dir = state_dir.join(INDEXES_DIR).join(&ws_id);
        if let Err(e) = fs::create_dir_all(&new_index_dir) {
            warn!(
                "Migration: failed to create index directory {:?}: {}",
                new_index_dir, e
            );
            continue;
        }

        if let Err(e) = save_index_sync(&new_index_dir, &index) {
            warn!("Migration: failed to save index {:?}: {}", new_index_dir, e);
            continue;
        }

        // Remove old position file (move semantics)
        if let Err(e) = fs::remove_file(&old_index_path) {
            warn!(
                "Migration: failed to remove old index file {:?}: {}",
                old_index_path, e
            );
        }

        info!("Legacy index migration completed for workspace {}", ws_id);

        workspace_entries.push(WorkspaceEntry {
            ws_id: ws_id.clone(),
            workspace_path: index.workspace_path.clone(),
            registered_at: chrono::Utc::now(),
            origin_backend: backend.backend_type(),
        });
        migrated_any = true;
    }

    if migrated_any {
        // Construct DaemonStateFile and save it
        let state_file = DaemonStateFile {
            version: DAEMON_STATE_VERSION,
            backend: BackendIdentity {
                backend_type: backend.backend_type(),
                selection_method: "auto-detect".to_string(),
                selected_at: chrono::Utc::now(),
            },
            paths: match backend.backend_type() {
                crate::backend::BackendType::BtrfsLoop => BackendPaths::BtrfsLoop {
                    mount_path: backend.data_root().to_path_buf(),
                    data_root: backend.data_root().to_path_buf(),
                    snapshots_root: backend.snapshots_root().to_path_buf(),
                    loop_img: None,
                },
                crate::backend::BackendType::BtrfsBase => BackendPaths::BtrfsBase {
                    mount_path: backend.data_root().to_path_buf(),
                    data_root: backend.data_root().to_path_buf(),
                    snapshots_root: backend.snapshots_root().to_path_buf(),
                },
                crate::backend::BackendType::OverlayFs => BackendPaths::OverlayFs {
                    data_root: backend.data_root().to_path_buf(),
                    snapshots_root: backend.snapshots_root().to_path_buf(),
                },
            },
            workspaces: workspace_entries,
        };
        if let Err(e) = persist::save_state(state_dir, &state_file) {
            warn!("Migration: failed to save state.json: {:#}", e);
        } else {
            info!("Migration complete, state.json generated");
        }
    }

    migrated_any
}
