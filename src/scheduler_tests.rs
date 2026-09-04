use super::*;
use std::path::{Path, PathBuf};
use std::sync::Barrier;

fn repo(name: &str, reserved: u32, max: u32) -> RepoConfig {
    RepoConfig {
        repo: name.into(),
        runtime: crate::config::RuntimeKind::Docker,
        labels: vec!["self-hosted".into()],
        image: "runner:test".into(),
        reserved,
        max,
        job_timeout_min: 60,
        caffeinate: false,
        registry_mirror: None,
    }
}

fn app(repos: Vec<RepoConfig>, max_total_runners: u32) -> Arc<App> {
    let config = Config {
        dashboard_port: 0,
        max_total_runners,
        data_dir: PathBuf::from("/tmp/homerunner-tests-unused"),
        keep_failed_workspaces: 0,
        failed_workspaces_max_age_days: None,
        failed_workspaces_max_bytes: None,
        keep_job_logs: 10,
        job_logs_max_age_days: None,
        job_logs_max_bytes: None,
        job_history_days: 365,
        event_history_days: 7,
        service_log_max_bytes: 10 * 1024 * 1024,
        service_log_backups: 3,
        poll_interval_s: 30,
        idle_decay_min: 10,
        auth_source: "env:HOMERUNNER_TEST_TOKEN".into(),
        python_version: "3.13".into(),
        node_version: "24".into(),
        repos,
        monitors: vec![],
    };
    App::new(
        config,
        GitHub::new("env:HOMERUNNER_TEST_TOKEN"),
        Store::open(Path::new(":memory:")).unwrap(),
        None,
    )
}

#[test]
fn concurrent_reservations_never_exceed_global_cap() {
    let repo = repo("owner/project", 0, 20);
    let app = app(vec![repo.clone()], 3);
    let barrier = Arc::new(Barrier::new(17));
    let handles: Vec<_> = (0..16)
        .map(|i| {
            let app = app.clone();
            let repo = repo.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                app.reserve_runner(
                    format!("runner-{i}"),
                    RunnerInfo::new(repo, RunnerState::Starting, String::new()),
                )
            })
        })
        .collect();
    barrier.wait();

    let reserved = handles
        .into_iter()
        .map(|handle| usize::from(handle.join().unwrap()))
        .sum::<usize>();
    assert_eq!(reserved, 3);
    assert_eq!(app.live_count(None), 3);
}

#[test]
fn reservations_enforce_repo_and_global_caps() {
    let first = repo("owner/first", 0, 2);
    let second = repo("owner/second", 0, 2);
    let app = app(vec![first.clone(), second.clone()], 3);

    for i in 0..2 {
        assert!(app.reserve_runner(
            format!("first-{i}"),
            RunnerInfo::new(first.clone(), RunnerState::Starting, String::new()),
        ));
    }
    assert!(!app.reserve_runner(
        "first-overflow".into(),
        RunnerInfo::new(first.clone(), RunnerState::Starting, String::new()),
    ));
    assert!(app.reserve_runner(
        "second-0".into(),
        RunnerInfo::new(second.clone(), RunnerState::Starting, String::new()),
    ));
    assert!(!app.reserve_runner(
        "second-global-overflow".into(),
        RunnerInfo::new(second, RunnerState::Starting, String::new()),
    ));
}

#[test]
fn snapshot_reports_effective_capacity() {
    let app = app(
        vec![repo("owner/first", 0, 3), repo("owner/second", 0, 2)],
        4,
    );
    let snapshot = app.snapshot();

    assert_eq!(snapshot["capacity"], 4);
    assert_eq!(snapshot["max_total_runners"], 4);
}

#[test]
fn log_replay_resumes_after_last_event_id() {
    let mut runner = RunnerInfo::new(
        repo("owner/project", 0, 1),
        RunnerState::Listening,
        "container".into(),
    );
    for i in 1..=5 {
        runner.push_log(format!("line {i}"));
    }

    let (replay, mut live) = runner.subscribe_logs(3);
    assert_eq!(replay, vec![(4, "line 4".into()), (5, "line 5".into())]);

    runner.push_log("line 6".into());
    assert_eq!(live.try_recv().unwrap(), (6, "line 6".into()));
}

#[test]
fn log_replay_keeps_only_the_recent_window() {
    let mut runner = RunnerInfo::new(
        repo("owner/project", 0, 1),
        RunnerState::Listening,
        "container".into(),
    );
    for i in 1..=505 {
        runner.push_log(format!("line {i}"));
    }

    let (replay, _) = runner.subscribe_logs(0);
    assert_eq!(replay.len(), 500);
    assert_eq!(replay.first().unwrap(), &(6, "line 6".into()));
    assert_eq!(replay.last().unwrap(), &(505, "line 505".into()));
}
