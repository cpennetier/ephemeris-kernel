//! Run manifest: per-run provenance emitted alongside the surface frames.
//!
//! A `RunManifest` records which build, scenario, seed, and schema produced a
//! run. It is written as JSON to `runs/{run_id}/run_manifest.json` at
//! simulation start and embedded in the surface stream's Arrow schema metadata
//! (key `run_manifest`) for browser-side provenance validation.
//!
//! v1 is single-threaded and unpartitioned; the partition/thread fields are
//! present (as `Option`/constants) so the run-lake schema is stable when the
//! parallel executor lands. Parquet emission for the run lake is a future
//! workstream — JSON is sufficient for v1.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;

/// Git SHA of the simulator build, stamped at compile time by `build.rs`.
/// In this monorepo the planner shares the same checkout, so `planner_git_sha`
/// uses the same value.
pub const SIMULATOR_GIT_SHA: &str = env!("SIMULATOR_GIT_SHA");

/// Per-run provenance record. Field order is the JSON key order.
#[derive(Debug, Clone, Serialize)]
pub struct RunManifest {
    pub run_id: String,
    pub scenario_id: String,
    /// "single_thread" for v1.
    pub execution_mode: String,
    pub seed: u64,
    /// Partition index for a partitioned run; `None` for v1.
    pub partition_id: Option<u32>,
    /// Hash of the partition assignment; `None` for v1.
    pub partition_hash: Option<String>,
    /// Worker thread count; 1 for v1.
    pub thread_count: u32,
    pub schema_version: String,
    pub schema_fingerprint: String,
    pub simulator_git_sha: String,
    pub planner_git_sha: String,
    /// SHA-256 of the controller config, or empty string when there is none.
    pub controller_config_hash: String,
    /// RFC 3339 timestamp. Supplied by the caller (the orchestrator owns the
    /// clock); the engine does not read wall-clock time here.
    pub created_at: String,
}

impl RunManifest {
    /// Build a v1 (single-thread, unpartitioned) manifest.
    #[allow(clippy::too_many_arguments)]
    pub fn v1(
        run_id: impl Into<String>,
        scenario_id: impl Into<String>,
        seed: u64,
        schema_version: impl Into<String>,
        schema_fingerprint: impl Into<String>,
        controller_config_hash: impl Into<String>,
        created_at: impl Into<String>,
    ) -> Self {
        RunManifest {
            run_id: run_id.into(),
            scenario_id: scenario_id.into(),
            execution_mode: "single_thread".to_string(),
            seed,
            partition_id: None,
            partition_hash: None,
            thread_count: 1,
            schema_version: schema_version.into(),
            schema_fingerprint: schema_fingerprint.into(),
            simulator_git_sha: SIMULATOR_GIT_SHA.to_string(),
            planner_git_sha: SIMULATOR_GIT_SHA.to_string(),
            controller_config_hash: controller_config_hash.into(),
            created_at: created_at.into(),
        }
    }

    /// Serialize to pretty JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("RunManifest serializes")
    }

    /// Write the manifest to `{runs_dir}/{run_id}/run_manifest.json`, creating
    /// the run directory. Returns the path written.
    pub fn write_to(&self, runs_dir: impl AsRef<Path>) -> std::io::Result<PathBuf> {
        let dir = runs_dir.as_ref().join(&self.run_id);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("run_manifest.json");
        let mut f = std::fs::File::create(&path)?;
        f.write_all(self.to_json().as_bytes())?;
        Ok(path)
    }
}

/// SHA-256 (full 64-hex) of a controller config blob. Empty input → empty
/// string (the "no controller" sentinel used in v1).
pub fn controller_config_hash(config: &[u8]) -> String {
    if config.is_empty() {
        return String::new();
    }
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(config);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_v1_has_required_fields_and_defaults() {
        let m = RunManifest::v1(
            "run-123", "scn-abc", 42, "2026.05.surface.v1", "deadbeefdeadbeef", "", "2026-05-29T00:00:00Z",
        );
        assert_eq!(m.execution_mode, "single_thread");
        assert_eq!(m.thread_count, 1);
        assert!(m.partition_id.is_none());
        assert!(m.partition_hash.is_none());
        let json = m.to_json();
        for key in [
            "run_id",
            "scenario_id",
            "execution_mode",
            "seed",
            "thread_count",
            "schema_version",
            "schema_fingerprint",
            "simulator_git_sha",
            "planner_git_sha",
            "controller_config_hash",
            "created_at",
        ] {
            assert!(json.contains(key), "manifest JSON missing key {key}");
        }
    }

    #[test]
    fn manifest_writes_to_run_dir() {
        let m = RunManifest::v1(
            "run-write-test", "scn", 1, "v", "fp", "", "2026-05-29T00:00:00Z",
        );
        let base = std::env::temp_dir().join("des_run_manifest_test");
        let _ = std::fs::remove_dir_all(&base);
        let path = m.write_to(&base).unwrap();
        assert!(path.ends_with("run-write-test/run_manifest.json"));
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("\"run_id\": \"run-write-test\""));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn controller_hash_empty_for_empty_config() {
        assert_eq!(controller_config_hash(&[]), "");
        assert_eq!(controller_config_hash(b"x").len(), 64);
    }
}
