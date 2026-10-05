#[path = "../examples/latency/observer.rs"]
mod observer;

#[test]
fn latency_observer_waits_for_complete_committed_effect() {
    let mut screen = observer::Observer::new(40, 5).expect("screen");
    screen.expect("probe-002-END".into(), 10);
    assert!(screen.feed(b"probe-001-END", 15).expect("stale").is_empty());
    assert!(screen
        .feed(b"\x1b[H\x1b[?2026hprobe-002-", 20)
        .expect("begin")
        .is_empty());
    assert!(screen.feed(b"END", 25).expect("partial").is_empty());
    let observed = screen.feed(b"\x1b[?2026l", 30).expect("commit");
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].marker, "probe-002-END");
    assert_eq!(observed[0].received_ns, 30);
    assert!(screen.feed(b"", 35).expect("duplicate").is_empty());
}

#[test]
fn latency_observer_rejects_rename_preview() {
    let mut screen = observer::Observer::new(40, 5).expect("screen");
    screen.expect("A000001".into(), 10);
    assert!(screen
        .feed(b"rename workspace\r\nA000001", 20)
        .expect("preview")
        .is_empty());
    let observed = screen
        .feed(b"\x1b[2J\x1b[HA000001", 30)
        .expect("authoritative");
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].received_ns, 30);
}

#[test]
fn latency_observer_preserves_multiple_frames_in_one_read() {
    let mut screen = observer::Observer::new(40, 5).expect("screen");
    screen.expect("probe-001-END".into(), 10);
    screen.expect("probe-002-END".into(), 10);
    let frames =
        b"\x1b[?2026h\x1b[Hprobe-001-END\x1b[?2026l\x1b[?2026h\x1b[Hprobe-002-END\x1b[?2026l";
    assert_eq!(screen.feed(frames, 30).expect("frames").len(), 2);
}

#[test]
fn latency_observer_records_each_panes_freshness() {
    let mut screen = observer::Observer::new(60, 5).expect("screen");
    screen
        .feed(
            b"\x1b[?2026h\x1b[HLOAD-1-0000000002\r\nLOAD-A-0000000008\x1b[?2026l",
            100,
        )
        .expect("load");
    screen
        .feed(b"\x1b[?2026h\x1b[HLOAD-1-0000000005\x1b[?2026l", 200)
        .expect("newer");
    assert_eq!(
        screen.load_observations,
        vec![
            ("1".into(), 2, 100),
            ("A".into(), 8, 100),
            ("1".into(), 5, 200)
        ]
    );
}

#[test]
fn latency_observer_does_not_complete_cancelled_probe() {
    let mut screen = observer::Observer::new(40, 5).expect("screen");
    screen.expect("cancelled-END".into(), 10);
    screen.cancel("cancelled-END");
    assert!(screen
        .feed(b"cancelled-END", 20)
        .expect("cancelled")
        .is_empty());
}
