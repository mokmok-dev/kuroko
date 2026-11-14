#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::{TcpListener, TcpStream};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use kuroko_core::{
    Book, Event, IssueId, LabelName, Limits, Source, TaskKey, TaskRequest, TaskSpec, TaskState,
    TriggerId, apply,
};
use kuroko_store::Store;
use tempfile::TempDir;

const WAIT: Duration = Duration::from_secs(20);

const fn limits(in_flight: usize, max_queued: usize) -> Limits {
    Limits {
        in_flight: NonZeroUsize::new(in_flight).unwrap(),
        max_queued: NonZeroUsize::new(max_queued).unwrap(),
    }
}

fn key(issue: &str) -> TaskKey {
    TaskKey {
        source: Source::Linear,
        issue: IssueId::new(issue),
        trigger: TriggerId::Label(LabelName::new("kuroko")),
    }
}

fn request(key: &TaskKey) -> TaskRequest {
    TaskRequest {
        key: key.clone(),
        spec: TaskSpec {
            identifier: key.issue.as_str().to_uppercase(),
            title: format!("title {}", key.issue.as_str()),
            description: None,
            url: None,
        },
        prompt: format!("work on {}", key.issue.as_str()),
    }
}

struct Fixture {
    _dir: TempDir,
    config: PathBuf,
    db: PathBuf,
    port: u16,
}

impl Fixture {
    /// Saves a book with one `Dispatched` blocker and one `Queued` target, so startup
    /// reconciliation leaves the target ready for `Resume` to dispatch.
    fn new(agent: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("tasks.db");
        let mut book = Book::new(limits(1, 4));
        let _ = apply(&mut book, Event::Admit(request(&key("issue-blocker"))));
        let _ = apply(&mut book, Event::Admit(request(&key("issue-target"))));
        Store::open(&db).unwrap().save(&book).unwrap();

        let port = free_port();
        let config = dir.path().join("kuroko.toml");
        std::fs::write(
            &config,
            format!(
                r#"
[server]
bind = "127.0.0.1:{port}"

[linear]
secret = "whsec_daemon_tests"

[store]
path = "tasks.db"

[transport.stdio]
command = "sh"
args = ["-c", "{agent}"]
timeout_secs = 30
"#
            ),
        )
        .unwrap();
        Self {
            _dir: dir,
            config,
            db,
            port,
        }
    }

    fn spawn(&self) -> Child {
        Command::new(env!("CARGO_BIN_EXE_kuroko"))
            .arg("--config")
            .arg(&self.config)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    fn wait_for_state(&self, key: &TaskKey, want: &TaskState) -> bool {
        let store = Store::open(&self.db).unwrap();
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            let loaded = store.load().unwrap();
            if loaded
                .tasks
                .iter()
                .any(|task| &task.key == key && &task.state == want)
            {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    }
}

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn wait_for_listener(child: &mut Child, port: u16) {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        assert!(
            child.try_wait().unwrap().is_none(),
            "the daemon exited before it listened"
        );
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            // Let `axum::serve` poll its shutdown future before a signal arrives.
            thread::sleep(Duration::from_millis(100));
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("the daemon did not listen on {port}");
}

fn signal(child: &Child, name: &str) {
    let status = Command::new("kill")
        .arg(format!("-{name}"))
        .arg(child.id().to_string())
        .status()
        .unwrap();
    assert!(status.success(), "kill -{name} failed");
}

fn wait_for_exit(child: &mut Child) -> Option<ExitStatus> {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        thread::sleep(Duration::from_millis(20));
    }
    None
}

#[test]
fn sigterm_persists_the_in_flight_result() {
    let fixture = Fixture::new("cat >/dev/null; sleep 2; echo done");
    let target = key("issue-target");
    let blocker = key("issue-blocker");
    let mut child = fixture.spawn();

    wait_for_listener(&mut child, fixture.port);
    assert!(
        fixture.wait_for_state(&target, &TaskState::Dispatched),
        "the restored queue was not dispatched"
    );
    signal(&child, "TERM");
    let status = wait_for_exit(&mut child).expect("the daemon did not exit after SIGTERM");
    assert!(status.success(), "SIGTERM exit was {status:?}");

    let loaded = Store::open(&fixture.db).unwrap().load().unwrap();
    let target = loaded.tasks.iter().find(|task| task.key == target).unwrap();
    match &target.state {
        TaskState::Succeeded(done) => {
            assert_eq!(done.outcome, kuroko_core::Outcome::Success);
            assert!(done.output.contains("done"), "output was {:?}", done.output);
        }
        other => panic!("expected the persisted success, got {other:?}"),
    }
    let blocker = loaded
        .tasks
        .iter()
        .find(|task| task.key == blocker)
        .unwrap();
    assert!(matches!(blocker.state, TaskState::RecoveryPending));
    assert_eq!(loaded.pending, Vec::new());
}

#[test]
fn sigkill_leaves_the_dispatch_for_recovery() {
    let fixture = Fixture::new("cat >/dev/null; sleep 5");
    let target = key("issue-target");
    let mut child = fixture.spawn();

    wait_for_listener(&mut child, fixture.port);
    assert!(
        fixture.wait_for_state(&target, &TaskState::Dispatched),
        "the restored queue was not dispatched"
    );
    signal(&child, "KILL");
    let status = wait_for_exit(&mut child).expect("the daemon did not exit after SIGKILL");
    assert!(!status.success(), "SIGKILL exit was {status:?}");

    let loaded = Store::open(&fixture.db).unwrap().load().unwrap();
    let target = loaded.tasks.iter().find(|task| task.key == target).unwrap();
    assert!(matches!(target.state, TaskState::Dispatched));
    let target = target.key.clone();

    let recovered = Book::reconcile(limits(1, 4), loaded.tasks, loaded.pending);
    assert_eq!(
        recovered.task(&target).map(|task| &task.state),
        Some(&TaskState::RecoveryPending)
    );
}
