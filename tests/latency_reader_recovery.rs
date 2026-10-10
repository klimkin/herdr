#[path = "../src/platform/latency.rs"]
mod platform;
#[path = "../examples/latency/reader_control.rs"]
mod reader_control;

#[test]
fn reader_pause_acknowledges_wait_and_records_reset_before_resume() {
    use std::sync::{mpsc, Arc};
    use std::time::Duration;
    let control = Arc::new(reader_control::ReaderControl::default());
    control.arm(2, 123, 500).expect("one pause armed");
    let reader = control.clone();
    let (progress_tx, progress_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        assert!(reader.before_read().expect("controlled read gate"));
        progress_tx.send(()).expect("reader resumed");
        reader.read_attempt().expect("read attempt recorded");
        reader.received(platform::monotonic_ns().expect("host clock"));
    });
    control
        .wait_paused(Duration::from_secs(1))
        .expect("actual reader wait acknowledged");
    assert!(matches!(
        progress_rx.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    control.reset().expect("controller resets delay");
    progress_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("parked reader wakes");
    worker.join().expect("reader worker");
    let lifecycle = control
        .snapshot()
        .expect("lifecycle")
        .expect("pause recorded");
    assert_eq!(lifecycle.client_index, 2);
    assert_eq!(lifecycle.client_pid, 123);
    assert_eq!(lifecycle.requested_ms, 500);
    assert!(lifecycle.installed_ns <= lifecycle.paused_ns.expect("paused"));
    assert!(lifecycle.paused_ns.expect("paused") <= lifecycle.reset_request_ns.expect("reset"));
    assert!(
        lifecycle.reset_request_ns.expect("reset")
            <= lifecycle.reset_complete_ns.expect("reset complete")
    );
    assert!(
        lifecycle.reset_request_ns.expect("reset")
            <= lifecycle.reader_resumed_ns.expect("reader resume")
    );
    assert!(
        lifecycle.reader_resumed_ns.expect("reader resume")
            <= lifecycle.first_read_attempt_ns.expect("attempt")
    );
    assert!(
        lifecycle.first_read_attempt_ns.expect("attempt")
            <= lifecycle.first_receipt_ns.expect("receipt")
    );
}

#[test]
fn reader_shutdown_wakes_pause_without_fabricating_recovery() {
    use std::sync::Arc;
    use std::time::Duration;
    let control = Arc::new(reader_control::ReaderControl::default());
    control.arm(1, 42, 1000).expect("arm");
    let reader = control.clone();
    let worker = std::thread::spawn(move || reader.before_read().expect("read gate"));
    control.wait_paused(Duration::from_secs(1)).expect("paused");
    control.stop();
    assert!(!worker.join().expect("stopped reader wakes"));
    let lifecycle = control.snapshot().expect("snapshot").expect("lifecycle");
    assert!(lifecycle.stopped);
    assert!(lifecycle.reset_request_ns.is_none());
    assert!(lifecycle.reader_resumed_ns.is_none());
    assert!(lifecycle.first_receipt_ns.is_none());
}

#[test]
fn reader_shutdown_interrupts_reset_timer() {
    use std::sync::{mpsc, Arc};
    use std::time::Duration;
    let control = Arc::new(reader_control::ReaderControl::default());
    control.arm(1, 42, 10_000).expect("arm bounded pause");
    let reader = control.clone();
    let read_worker = std::thread::spawn(move || reader.before_read().expect("reader gate"));
    control.wait_paused(Duration::from_secs(1)).expect("paused");
    let timer = control.clone();
    let (done_tx, done_rx) = mpsc::channel();
    let reset_worker = std::thread::spawn(move || {
        timer
            .reset_after(Duration::from_secs(10))
            .expect("stopped timer exits");
        done_tx.send(()).expect("timer done");
    });
    control.stop();
    done_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("timer wakes on stop");
    reset_worker.join().expect("reset worker joined");
    assert!(!read_worker.join().expect("read worker joined"));
}
