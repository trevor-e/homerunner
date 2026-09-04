//! Reconcile retained artifacts with the journal and enforce storage policy.

use crate::config::{Config, RuntimeKind};
use crate::store::{ArtifactRecord, DockerCacheRecord, Store};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const DAY: f64 = 86_400.0;

#[derive(Debug, Default, Serialize)]
pub struct CleanupReport {
    pub dry_run: bool,
    pub planned: usize,
    pub removed: usize,
    pub reclaimed_bytes: u64,
    pub repaired: usize,
    pub stale_references: usize,
    pub pruned_jobs: usize,
    pub pruned_events: usize,
    pub actions: Vec<String>,
    pub errors: Vec<String>,
}

impl CleanupReport {
    pub fn summary(&self) -> String {
        let errors = if self.errors.is_empty() {
            String::new()
        } else {
            format!("; {} error(s)", self.errors.len())
        };
        if self.dry_run {
            format!(
                "would remove {} artifact(s) and reclaim {}; would repair {} reference(s) and prune {} job(s) and {} event(s){errors}",
                self.planned,
                human_bytes(self.reclaimed_bytes),
                self.repaired,
                self.pruned_jobs,
                self.pruned_events,
            )
        } else {
            format!(
                "removed {} artifact(s), reclaimed {}; repaired {} reference(s), pruned {} job(s) and {} event(s){errors}",
                self.removed,
                human_bytes(self.reclaimed_bytes),
                self.repaired,
                self.pruned_jobs,
                self.pruned_events,
            )
        }
    }
}

#[derive(Debug)]
struct Candidate {
    name: String,
    bytes: u64,
    timestamp: f64,
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn human_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn dir_bytes(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| {
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => dir_bytes(&path),
                Ok(kind) if kind.is_file() => entry.metadata().map(|m| m.len()).unwrap_or(0),
                _ => 0,
            }
        })
        .sum()
}

/// Select oldest artifacts until every configured constraint is satisfied.
fn select_removals(
    candidates: &[Candidate],
    keep_count: usize,
    max_age_days: Option<u64>,
    max_bytes: Option<u64>,
    ts: f64,
) -> HashSet<usize> {
    let mut selected = HashSet::new();
    if let Some(days) = max_age_days {
        let cutoff = ts - days as f64 * DAY;
        for (index, candidate) in candidates.iter().enumerate() {
            if candidate.timestamp < cutoff {
                selected.insert(index);
            }
        }
    }

    let mut remaining: Vec<usize> = (0..candidates.len())
        .filter(|index| !selected.contains(index))
        .collect();
    remaining.sort_by(|a, b| {
        candidates[*a]
            .timestamp
            .total_cmp(&candidates[*b].timestamp)
    });
    while remaining.len() > keep_count {
        selected.insert(remaining.remove(0));
    }

    if let Some(max_bytes) = max_bytes {
        let mut bytes: u64 = remaining.iter().map(|i| candidates[*i].bytes).sum();
        while bytes > max_bytes && !remaining.is_empty() {
            let oldest = remaining.remove(0);
            bytes = bytes.saturating_sub(candidates[oldest].bytes);
            selected.insert(oldest);
        }
    }
    selected
}

#[derive(Debug)]
struct LogDir {
    candidate: Candidate,
    path: PathBuf,
    job_id: Option<i64>,
}

fn remove_log_dir_with(
    store: &Mutex<Store>,
    dir: &LogDir,
    report: &mut CleanupReport,
    remove: impl FnOnce(&Path) -> std::io::Result<()>,
) {
    match remove(&dir.path) {
        Ok(()) => {
            report.removed += 1;
            report.reclaimed_bytes += dir.candidate.bytes;
            if let Err(error) = store
                .lock()
                .unwrap()
                .clear_log_dir(&dir.path.to_string_lossy())
            {
                report.errors.push(format!(
                    "removed {} but could not clear its journal reference: {error}",
                    dir.path.display()
                ));
            }
        }
        Err(error) => report.errors.push(format!(
            "remove log directory {}: {error}",
            dir.path.display()
        )),
    }
}

fn inventory_log_dirs(jobs_dir: &Path) -> Vec<LogDir> {
    let Ok(entries) = std::fs::read_dir(jobs_dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| {
            let path = entry.path();
            let timestamp = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                .map_or(0.0, |duration| duration.as_secs_f64());
            let job_id = entry.file_name().to_string_lossy().parse().ok();
            LogDir {
                candidate: Candidate {
                    name: path.display().to_string(),
                    bytes: dir_bytes(&path),
                    timestamp,
                },
                path,
                job_id,
            }
        })
        .collect()
}

fn cleanup_logs(
    config: &Config,
    store: &Mutex<Store>,
    records: &[ArtifactRecord],
    dry_run: bool,
    report: &mut CleanupReport,
) {
    let dirs = inventory_log_dirs(&config.data_dir.join("jobs"));
    let actual: HashSet<String> = dirs
        .iter()
        .map(|dir| dir.path.to_string_lossy().into_owned())
        .collect();
    for record in records {
        if let Some(path) = record.log_dir.as_deref() {
            if !actual.contains(path) {
                report.stale_references += 1;
                report
                    .actions
                    .push(format!("clear missing log reference {path}"));
                if !dry_run {
                    if let Err(error) = store.lock().unwrap().clear_log_dir(path) {
                        report
                            .errors
                            .push(format!("clear log reference {path}: {error}"));
                    }
                }
            }
        }
    }

    let by_job: HashMap<i64, &ArtifactRecord> = records
        .iter()
        .map(|record| (record.job_id, record))
        .collect();
    for dir in &dirs {
        let Some(record) = dir.job_id.and_then(|job_id| by_job.get(&job_id)) else {
            continue;
        };
        if record.log_dir.is_none() {
            report.actions.push(format!(
                "attach log directory {} to job {}",
                dir.path.display(),
                record.job_id
            ));
            if dry_run {
                report.repaired += 1;
            } else {
                match store
                    .lock()
                    .unwrap()
                    .repair_log_dir(record.job_id, &dir.path.to_string_lossy())
                {
                    Ok(changed) => report.repaired += changed,
                    Err(error) => report.errors.push(format!(
                        "attach log directory {}: {error}",
                        dir.path.display()
                    )),
                }
            }
        }
    }

    let candidates: Vec<Candidate> = dirs
        .iter()
        .map(|dir| Candidate {
            name: dir.candidate.name.clone(),
            bytes: dir.candidate.bytes,
            timestamp: dir
                .job_id
                .and_then(|job_id| by_job.get(&job_id))
                .and_then(|record| record.completed_at)
                .unwrap_or(dir.candidate.timestamp),
        })
        .collect();
    let selected = select_removals(
        &candidates,
        config.keep_job_logs as usize,
        config.job_logs_max_age_days,
        config.job_logs_max_bytes,
        now(),
    );
    for index in selected {
        let dir = &dirs[index];
        report.planned += 1;
        report.actions.push(format!(
            "{} log directory {} ({})",
            if dry_run { "remove" } else { "removed" },
            dir.candidate.name,
            human_bytes(dir.candidate.bytes)
        ));
        if dry_run {
            report.reclaimed_bytes += dir.candidate.bytes;
            continue;
        }
        remove_log_dir_with(store, dir, report, |path| std::fs::remove_dir_all(path));
    }
}

fn runtime_named(name: &str) -> Option<RuntimeKind> {
    match name {
        "docker" => Some(RuntimeKind::Docker),
        "apple-container" => Some(RuntimeKind::AppleContainer),
        _ => None,
    }
}

fn image_job_id(tag: &str) -> Option<i64> {
    tag.strip_prefix("homerunner-kept:")?.parse().ok()
}

fn cache_removal_reason(
    in_use: bool,
    configured_slots: Option<u32>,
    slot: u32,
    last_used: f64,
    max_age_days: u64,
    ts: f64,
) -> Option<&'static str> {
    if in_use {
        return None;
    }
    if configured_slots.is_none_or(|max| slot >= max) {
        return Some("no configured cache slot");
    }
    if max_age_days > 0 && last_used < ts - max_age_days as f64 * DAY {
        return Some("expired");
    }
    None
}

#[derive(Debug)]
struct ImageArtifact {
    candidate: Candidate,
    runtime: RuntimeKind,
    job_id: Option<i64>,
}

fn record_image_removal(
    store: &Mutex<Store>,
    image: &ImageArtifact,
    result: anyhow::Result<()>,
    report: &mut CleanupReport,
) {
    match result {
        Ok(()) => {
            report.removed += 1;
            report.reclaimed_bytes += image.candidate.bytes;
            if let Some(job_id) = image.job_id {
                if let Err(error) = store.lock().unwrap().clear_kept_image(job_id) {
                    report.errors.push(format!(
                        "removed {} but could not clear its journal reference: {error}",
                        image.candidate.name
                    ));
                }
            }
        }
        Err(error) => report.errors.push(format!(
            "remove {} image {}: {error}",
            image.runtime.name(),
            image.candidate.name
        )),
    }
}

fn associate_image_jobs(images: &mut [ImageArtifact], records: &[ArtifactRecord]) {
    let tracked_by_image: HashMap<(RuntimeKind, String), i64> = records
        .iter()
        .filter_map(|record| {
            Some((
                (
                    runtime_named(record.kept_image_runtime.as_deref()?)?,
                    record.kept_image.clone()?,
                ),
                record.job_id,
            ))
        })
        .collect();
    for image in images {
        image.job_id = tracked_by_image
            .get(&(image.runtime, image.candidate.name.clone()))
            .copied()
            .or_else(|| image_job_id(&image.candidate.name));
    }
}

async fn cleanup_images(
    config: &Config,
    store: &Mutex<Store>,
    records: &[ArtifactRecord],
    dry_run: bool,
    report: &mut CleanupReport,
) {
    let mut runtimes: HashSet<RuntimeKind> = config.repos.iter().map(|repo| repo.runtime).collect();
    runtimes.extend(
        records
            .iter()
            .filter_map(|record| record.kept_image_runtime.as_deref())
            .filter_map(runtime_named),
    );
    let mut inventoried = HashSet::new();
    let mut images = Vec::new();
    for runtime in runtimes {
        match runtime.list_kept_images().await {
            Ok(found) => {
                inventoried.insert(runtime);
                images.extend(found.into_iter().map(|image| ImageArtifact {
                    job_id: image_job_id(&image.tag),
                    candidate: Candidate {
                        name: image.tag,
                        bytes: image.size_bytes,
                        timestamp: 0.0,
                    },
                    runtime,
                }));
            }
            Err(error) => report
                .errors
                .push(format!("inventory {} kept images: {error}", runtime.name())),
        }
    }

    associate_image_jobs(&mut images, records);

    let actual: HashSet<(RuntimeKind, String)> = images
        .iter()
        .map(|image| (image.runtime, image.candidate.name.clone()))
        .collect();
    for record in records {
        let (Some(tag), Some(runtime_name)) = (
            record.kept_image.as_deref(),
            record.kept_image_runtime.as_deref(),
        ) else {
            continue;
        };
        let Some(runtime) = runtime_named(runtime_name) else {
            report.errors.push(format!(
                "job {} has unknown image runtime {runtime_name}",
                record.job_id
            ));
            continue;
        };
        if inventoried.contains(&runtime) && !actual.contains(&(runtime, tag.to_string())) {
            report.stale_references += 1;
            report
                .actions
                .push(format!("clear missing image reference {tag}"));
            if !dry_run {
                if let Err(error) = store.lock().unwrap().clear_kept_image(record.job_id) {
                    report
                        .errors
                        .push(format!("clear image reference {tag}: {error}"));
                }
            }
        }
    }

    let by_job: HashMap<i64, &ArtifactRecord> = records
        .iter()
        .map(|record| (record.job_id, record))
        .collect();
    for image in &images {
        let Some(record) = image.job_id.and_then(|job_id| by_job.get(&job_id)) else {
            continue;
        };
        if record.kept_image.is_none() {
            report.actions.push(format!(
                "attach image {} to job {}",
                image.candidate.name, record.job_id
            ));
            if dry_run {
                report.repaired += 1;
            } else {
                match store.lock().unwrap().repair_kept_image(
                    record.job_id,
                    &image.candidate.name,
                    image.runtime.name(),
                ) {
                    Ok(changed) => report.repaired += changed,
                    Err(error) => report
                        .errors
                        .push(format!("attach image {}: {error}", image.candidate.name)),
                }
            }
        }
    }

    let candidates: Vec<Candidate> = images
        .iter()
        .map(|image| Candidate {
            name: image.candidate.name.clone(),
            bytes: image.candidate.bytes,
            timestamp: image
                .job_id
                .and_then(|job_id| by_job.get(&job_id))
                .and_then(|record| record.completed_at)
                .unwrap_or(0.0),
        })
        .collect();
    let selected = select_removals(
        &candidates,
        config.keep_failed_workspaces as usize,
        config.failed_workspaces_max_age_days,
        config.failed_workspaces_max_bytes,
        now(),
    );
    for index in selected {
        let image = &images[index];
        report.planned += 1;
        report.actions.push(format!(
            "{} {} image {} ({})",
            if dry_run { "remove" } else { "removed" },
            image.runtime.name(),
            image.candidate.name,
            human_bytes(image.candidate.bytes)
        ));
        if dry_run {
            report.reclaimed_bytes += image.candidate.bytes;
            continue;
        }
        let result = image.runtime.remove_image(&image.candidate.name).await;
        record_image_removal(store, image, result, report);
    }
}

async fn cleanup_docker_caches(
    config: &Config,
    store: &Mutex<Store>,
    dry_run: bool,
    report: &mut CleanupReport,
) {
    let records = store.lock().unwrap().docker_cache_records();
    if !config.repos.iter().any(|repo| repo.docker_layer_cache) && records.is_empty() {
        return;
    }
    let volumes = match RuntimeKind::Docker.list_docker_cache_volumes().await {
        Ok(volumes) => volumes,
        Err(error) => {
            report
                .errors
                .push(format!("inventory Docker layer caches: {error}"));
            return;
        }
    };
    let actual: HashSet<&str> = volumes.iter().map(|volume| volume.name.as_str()).collect();
    for record in &records {
        if !actual.contains(record.name.as_str()) {
            report.stale_references += 1;
            report
                .actions
                .push(format!("forget missing Docker cache {}", record.name));
            if !dry_run {
                if let Err(error) = store.lock().unwrap().forget_docker_cache(&record.name) {
                    report
                        .errors
                        .push(format!("forget Docker cache {}: {error}", record.name));
                }
            }
        }
    }

    let tracked: HashMap<&str, &DockerCacheRecord> = records
        .iter()
        .map(|record| (record.name.as_str(), record))
        .collect();
    let configured: HashMap<&str, u32> = config
        .repos
        .iter()
        .filter(|repo| repo.docker_layer_cache)
        .map(|repo| (repo.repo.as_str(), repo.max))
        .collect();
    for volume in volumes {
        let Some(record) = tracked.get(volume.name.as_str()).copied() else {
            report.repaired += 1;
            report.actions.push(format!(
                "track existing Docker cache {} for {} slot {}",
                volume.name, volume.repo, volume.slot
            ));
            if !dry_run {
                store
                    .lock()
                    .unwrap()
                    .touch_docker_cache(&volume.name, &volume.repo, volume.slot);
            }
            continue;
        };
        let reason = cache_removal_reason(
            volume.in_use,
            configured.get(volume.repo.as_str()).copied(),
            volume.slot,
            record.last_used,
            config.docker_cache_max_age_days,
            now(),
        );
        let Some(reason) = reason else {
            continue;
        };
        report.planned += 1;
        report.actions.push(format!(
            "{} Docker cache {} ({reason})",
            if dry_run { "remove" } else { "removed" },
            volume.name
        ));
        if dry_run {
            continue;
        }
        match RuntimeKind::Docker
            .remove_docker_cache_volume(&volume.name)
            .await
        {
            Ok(()) => {
                report.removed += 1;
                if let Err(error) = store.lock().unwrap().forget_docker_cache(&volume.name) {
                    report.errors.push(format!(
                        "removed Docker cache {} but could not forget it: {error}",
                        volume.name
                    ));
                }
            }
            Err(error) => report
                .errors
                .push(format!("remove Docker cache {}: {error}", volume.name)),
        }
    }
}

pub async fn run(config: &Config, store: &Mutex<Store>, dry_run: bool) -> CleanupReport {
    let mut report = CleanupReport {
        dry_run,
        ..CleanupReport::default()
    };
    let records = store.lock().unwrap().artifact_records();
    cleanup_logs(config, store, &records, dry_run, &mut report);
    cleanup_images(config, store, &records, dry_run, &mut report).await;
    cleanup_docker_caches(config, store, dry_run, &mut report).await;

    let job_age = config.job_history_days as f64 * DAY;
    let event_age = config.event_history_days as f64 * DAY;
    if dry_run {
        let store = store.lock().unwrap();
        report.pruned_jobs = store.count_prunable_jobs(job_age);
        report.pruned_events = store.count_prunable_events(event_age);
    } else {
        let store = store.lock().unwrap();
        match store.prune_jobs(job_age) {
            Ok(count) => report.pruned_jobs = count,
            Err(error) => report.errors.push(format!("prune job history: {error}")),
        }
        match store.prune_events(event_age) {
            Ok(count) => report.pruned_events = count,
            Err(error) => report.errors.push(format!("prune event history: {error}")),
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RepoConfig;
    use crate::test_support::TempDir;
    use serde_json::json;

    fn candidate(timestamp: f64, bytes: u64) -> Candidate {
        Candidate {
            name: timestamp.to_string(),
            bytes,
            timestamp,
        }
    }

    #[test]
    fn cache_cleanup_respects_use_config_and_age() {
        let ts = 100.0 * DAY;
        assert_eq!(
            cache_removal_reason(false, Some(2), 2, ts, 30, ts),
            Some("no configured cache slot")
        );
        assert_eq!(
            cache_removal_reason(false, Some(2), 1, 60.0 * DAY, 30, ts),
            Some("expired")
        );
        assert_eq!(
            cache_removal_reason(false, Some(2), 1, 60.0 * DAY, 0, ts),
            None
        );
        assert_eq!(cache_removal_reason(true, None, 99, 0.0, 1, ts), None);
    }

    fn config(data_dir: &Path, keep_job_logs: u32) -> Config {
        Config {
            dashboard_port: 0,
            max_total_runners: 1,
            data_dir: data_dir.into(),
            keep_failed_workspaces: 0,
            failed_workspaces_max_age_days: None,
            failed_workspaces_max_bytes: None,
            keep_job_logs,
            job_logs_max_age_days: None,
            job_logs_max_bytes: None,
            job_history_days: 365,
            event_history_days: 7,
            service_log_max_bytes: 1024,
            service_log_backups: 1,
            poll_interval_s: 30,
            idle_decay_min: 10,
            docker_cache_max_age_days: 30,
            auth_source: "env:TEST_TOKEN".into(),
            python_version: "3.13".into(),
            node_version: "24".into(),
            monitors: vec![],
            repos: vec![RepoConfig {
                repo: "owner/repo".into(),
                runtime: RuntimeKind::AppleContainer,
                labels: Vec::new(),
                image: "runner:test".into(),
                reserved: 0,
                max: 1,
                job_timeout_min: 60,
                caffeinate: false,
                registry_mirror: None,
                docker_layer_cache: false,
            }],
        }
    }

    fn record_job(store: &Store, id: i64) {
        store.job_started(
            &json!({"job_id": id, "job_name": "test"}),
            "owner/repo",
            "runner",
            Some(1.0),
        );
        store.job_concluded(id, "failure");
    }

    #[test]
    fn policy_combines_age_count_and_size_limits() {
        let candidates = vec![
            candidate(1.0, 10),
            candidate(50.0, 10),
            candidate(60.0, 10),
            candidate(70.0, 10),
        ];
        let selected = select_removals(&candidates, 3, Some(1), Some(15), DAY + 55.0);
        assert_eq!(selected, HashSet::from([0, 1, 2]));
    }

    #[test]
    fn zero_count_selects_every_artifact() {
        let candidates = vec![candidate(1.0, 1), candidate(2.0, 1)];
        assert_eq!(
            select_removals(&candidates, 0, None, None, 3.0),
            HashSet::from([0, 1])
        );
    }

    #[test]
    fn image_tags_map_only_standard_numeric_jobs() {
        assert_eq!(image_job_id("homerunner-kept:42"), Some(42));
        assert_eq!(image_job_id("homerunner-kept:runner-name"), None);
        assert_eq!(image_job_id("unrelated:42"), None);
    }

    #[test]
    fn image_association_uses_the_recorded_runtime_and_supports_runner_tags() {
        let records = vec![ArtifactRecord {
            job_id: 9,
            completed_at: Some(10.0),
            log_dir: None,
            kept_image: Some("homerunner-kept:runner-name".into()),
            kept_image_runtime: Some("docker".into()),
        }];
        let mut images = vec![
            ImageArtifact {
                candidate: candidate(0.0, 1),
                runtime: RuntimeKind::Docker,
                job_id: None,
            },
            ImageArtifact {
                candidate: candidate(0.0, 1),
                runtime: RuntimeKind::AppleContainer,
                job_id: None,
            },
        ];
        for image in &mut images {
            image.candidate.name = "homerunner-kept:runner-name".into();
        }

        associate_image_jobs(&mut images, &records);

        assert_eq!(images[0].job_id, Some(9));
        assert_eq!(images[1].job_id, None);
    }

    #[test]
    fn log_cleanup_repairs_then_removes_journal_reference() {
        let dir = TempDir::new("cleanup-reconcile");
        let jobs = dir.path().join("jobs/42");
        std::fs::create_dir_all(&jobs).unwrap();
        std::fs::write(jobs.join("meta.json"), "{}").unwrap();
        let store = Mutex::new(Store::open(Path::new(":memory:")).unwrap());
        record_job(&store.lock().unwrap(), 42);

        let records = store.lock().unwrap().artifact_records();
        let mut report = CleanupReport::default();
        cleanup_logs(&config(dir.path(), 1), &store, &records, false, &mut report);
        assert_eq!(report.repaired, 1);
        assert_eq!(
            store.lock().unwrap().artifact_records()[0]
                .log_dir
                .as_deref(),
            Some(jobs.to_string_lossy().as_ref())
        );

        let records = store.lock().unwrap().artifact_records();
        cleanup_logs(&config(dir.path(), 0), &store, &records, false, &mut report);
        assert!(!jobs.exists());
        assert!(store.lock().unwrap().artifact_records()[0]
            .log_dir
            .is_none());
    }

    #[test]
    fn dry_run_does_not_repair_or_remove_logs() {
        let dir = TempDir::new("cleanup-dry-run");
        let jobs = dir.path().join("jobs/43");
        std::fs::create_dir_all(&jobs).unwrap();
        let store = Mutex::new(Store::open(Path::new(":memory:")).unwrap());
        record_job(&store.lock().unwrap(), 43);
        let records = store.lock().unwrap().artifact_records();
        let mut report = CleanupReport::default();

        cleanup_logs(&config(dir.path(), 0), &store, &records, true, &mut report);

        assert_eq!(report.repaired, 1);
        assert_eq!(report.planned, 1);
        assert!(jobs.exists());
        assert!(store.lock().unwrap().artifact_records()[0]
            .log_dir
            .is_none());
    }

    #[test]
    fn failed_deletion_preserves_journal_reference() {
        let store = Mutex::new(Store::open(Path::new(":memory:")).unwrap());
        record_job(&store.lock().unwrap(), 7);
        store
            .lock()
            .unwrap()
            .set_job_artifacts(7, Some("/logs/7"), None, None);
        let dir = LogDir {
            candidate: candidate(1.0, 10),
            path: PathBuf::from("/logs/7"),
            job_id: Some(7),
        };
        let mut report = CleanupReport::default();

        remove_log_dir_with(&store, &dir, &mut report, |_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "denied",
            ))
        });

        assert_eq!(report.removed, 0);
        assert_eq!(report.errors.len(), 1);
        assert_eq!(
            store.lock().unwrap().artifact_records()[0]
                .log_dir
                .as_deref(),
            Some("/logs/7")
        );
    }

    #[test]
    fn failed_image_deletion_preserves_journal_reference() {
        let store = Mutex::new(Store::open(Path::new(":memory:")).unwrap());
        record_job(&store.lock().unwrap(), 8);
        store
            .lock()
            .unwrap()
            .set_job_artifacts(8, None, Some("homerunner-kept:8"), Some("docker"));
        let image = ImageArtifact {
            candidate: Candidate {
                name: "homerunner-kept:8".into(),
                bytes: 100,
                timestamp: 1.0,
            },
            runtime: RuntimeKind::Docker,
            job_id: Some(8),
        };
        let mut report = CleanupReport::default();

        record_image_removal(
            &store,
            &image,
            Err(anyhow::anyhow!("runtime refused deletion")),
            &mut report,
        );

        assert_eq!(report.removed, 0);
        assert_eq!(report.errors.len(), 1);
        assert_eq!(
            store.lock().unwrap().artifact_records()[0]
                .kept_image
                .as_deref(),
            Some("homerunner-kept:8")
        );
    }
}
