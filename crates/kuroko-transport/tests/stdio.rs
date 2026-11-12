#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};

use kuroko_core::{
    Dispatch, FailureReason, IssueId, LabelName, Millis, Outcome, Source, TaskKey, TriggerId,
};
use kuroko_engine::Transport;
use kuroko_transport::Stdio;

const TIMEOUT: Duration = Duration::from_secs(10);
const CAP: usize = 8 * 1024;

fn dispatch(prompt: &str) -> Dispatch {
    Dispatch {
        key: TaskKey {
            source: Source::Linear,
            issue: IssueId::new("issue-a"),
            trigger: TriggerId::Label(LabelName::new("kuroko")),
        },
        issue: "MOK-1".to_owned(),
        prompt: prompt.to_owned(),
    }
}

fn shell(script: &str) -> Stdio {
    Stdio {
        command: "sh".to_owned(),
        args: vec!["-c".to_owned(), script.to_owned()],
        timeout: TIMEOUT,
    }
}

fn is_gone(pid: &str) -> bool {
    #[cfg(target_os = "linux")]
    {
        !std::path::Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::process::Command::new("kill")
            .args(["-0", pid])
            .status()
            .is_ok_and(|status| !status.success())
    }
}

#[tokio::test]
async fn the_prompt_reaches_the_child_on_stdin() {
    let job = dispatch("first line\nsecond line\n");
    let done = shell("cat").run(&job).await;

    assert_eq!(done.outcome, Outcome::Success);
    assert_eq!(done.output, job.prompt);
    assert_eq!(done.log, "");
    assert_eq!(done.at, Millis(0));
}

#[tokio::test]
async fn a_zero_exit_is_success() {
    let done = shell("exit 0").run(&dispatch("ignored")).await;

    assert_eq!(done.outcome, Outcome::Success);
    assert_eq!(done.output, "");
}

#[tokio::test]
async fn a_nonzero_exit_carries_its_code() {
    let done = shell("exit 3").run(&dispatch("ignored")).await;

    assert_eq!(
        done.outcome,
        Outcome::Failure(FailureReason::Exit { code: Some(3) })
    );
}

#[tokio::test]
async fn a_signalled_child_has_no_exit_code() {
    let done = shell("kill -9 $$").run(&dispatch("ignored")).await;

    assert_eq!(
        done.outcome,
        Outcome::Failure(FailureReason::Exit { code: None })
    );
}

#[tokio::test]
async fn a_missing_binary_is_disconnected() {
    let transport = Stdio {
        command: "kuroko-no-such-agent".to_owned(),
        args: Vec::new(),
        timeout: TIMEOUT,
    };
    let done = transport.run(&dispatch("ignored")).await;

    assert_eq!(done.outcome, Outcome::Failure(FailureReason::Disconnected));
    assert!(
        done.log.contains("kuroko-no-such-agent"),
        "log was {:?}",
        done.log
    );
}

#[tokio::test]
async fn a_child_that_outlives_the_timeout_is_killed_and_reaped() {
    let path = std::env::temp_dir().join(format!("kuroko-transport-{}.pid", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let transport = Stdio {
        command: "sh".to_owned(),
        args: vec![
            "-c".to_owned(),
            format!("echo $$ > '{}'; exec sleep 30", path.display()),
        ],
        timeout: Duration::from_secs(1),
    };

    let started = Instant::now();
    let done = transport.run(&dispatch("ignored")).await;
    let elapsed = started.elapsed();
    assert_eq!(done.outcome, Outcome::Failure(FailureReason::Timeout));
    assert!(elapsed < Duration::from_secs(10), "took {elapsed:?}");

    let pid = std::fs::read_to_string(&path).unwrap();
    let pid = pid.trim();
    assert!(is_gone(pid), "pid {pid} outlived the timeout");

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn output_past_the_cap_keeps_the_tail() {
    // Larger than a pipe buffer, so the child only makes progress while its output is
    // drained during the write.
    let prompt: String = "abcdefghijklmnopqrstuvwxyz"
        .chars()
        .cycle()
        .take(200_000)
        .collect();
    let done = shell("cat").run(&dispatch(&prompt)).await;

    assert_eq!(done.outcome, Outcome::Success);
    assert_eq!(done.output, prompt[prompt.len() - CAP..]);
}

#[tokio::test]
async fn a_multibyte_tail_is_cut_on_a_char_boundary() {
    let mut prompt = String::from("€");
    prompt.push_str(&"a".repeat(CAP - 2));
    let done = shell("cat").run(&dispatch(&prompt)).await;

    assert_eq!(done.outcome, Outcome::Success);
    assert_eq!(done.output, "a".repeat(CAP - 2));
}

#[tokio::test]
async fn stderr_lands_in_the_log() {
    let done = shell("echo oops >&2").run(&dispatch("ignored")).await;

    assert_eq!(done.outcome, Outcome::Success);
    assert_eq!(done.output, "");
    assert_eq!(done.log, "oops\n");
}
