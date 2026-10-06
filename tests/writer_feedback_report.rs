#[test]
fn writer_feedback_report_distinguishes_slots_from_completed_writes() {
    let script = r#"
import sys
sys.path.insert(0, 'scripts')
from writer_feedback_report import summarize
records = [
    {'stage':'writer.client','pid':1,'id':7,'scope':11,'value':0,'ns':10},
    {'stage':'writer.dequeue.render','pid':1,'id':0,'scope':11,'value':100,'ns':20},
    {'stage':'writer.feedback.begin','pid':1,'id':7,'scope':11,'value':64,'ns':21},
    {'stage':'writer.feedback.end','pid':1,'id':7,'scope':11,'value':1,'ns':40},
    {'stage':'writer.feedback.handled','pid':1,'id':7,'scope':11,'value':0,'ns':41},
    {'stage':'writer.dequeue.render','pid':1,'id':0,'scope':11,'value':200,'ns':50},
    {'stage':'writer.feedback.begin','pid':1,'id':7,'scope':11,'value':0,'ns':51},
    {'stage':'writer.feedback.end','pid':1,'id':7,'scope':11,'value':1,'ns':52},
    {'stage':'writer.feedback.handled','pid':1,'id':7,'scope':11,'value':1,'ns':53},
    {'stage':'writer.write_complete.render','pid':1,'id':0,'scope':11,'value':200,'ns':60},
    {'stage':'writer.queue.occupancy','pid':1,'id':4,'scope':11,'value':3,'ns':15},
    {'stage':'writer.dequeue.render','pid':1,'id':0,'scope':21,'value':900,'ns':25},
]
result = summarize(records)
client = next(row for row in result['clients'] if row['scope']==11)
assert client['client_id']==7
assert client['render_dequeues']==2
assert client['render_write_completions']==1
assert client['render_written_bytes']==200
assert client['feedback_sent']==2
assert client['feedback_without_deferred_work']==1
assert client['feedback_requesting_retry']==1
assert client['feedback_send_wait_ns']==20
assert client['event_channel_high_water']==64
assert client['render_lane_high_water']==3
assert client['control_lane_high_water']==4
other = next(row for row in result['clients'] if row['scope']==21)
assert other['client_id'] is None
assert other['render_write_completions']==0
assert result['complete'] is False
"#;
    let status = std::process::Command::new("python3")
        .args(["-c", script])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .expect("writer feedback report executable");
    assert!(status.success());
}

#[test]
fn writer_feedback_report_requires_every_expected_process_to_finish() {
    let script = r#"
import sys
sys.path.insert(0, 'scripts')
from writer_feedback_report import summarize
records = [{'stage':'process.finish','pid':1,'ns':100,'dropped':0}]
assert summarize(records)['complete'] is False
assert summarize(records, [1])['complete'] is True
assert summarize(records, [1,2])['complete'] is False
assert summarize(records, [1,2])['missing_processes'] == [2]
assert summarize(records, [1,2])['missing_finish_processes'] == [2]
records.append({'stage':'writer.client','pid':2,'id':3,'scope':4,'value':0,'ns':1,'dropped':1})
assert summarize(records, [1,2])['complete'] is False
records.append({'stage':'process.finish','pid':2,'ns':200,'dropped':1})
assert summarize(records, [1,2])['complete'] is False
"#;
    let status = std::process::Command::new("python3")
        .args(["-c", script])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .expect("writer feedback report executable");
    assert!(status.success());
}

#[test]
fn writer_feedback_report_identifies_overlapping_notifications() {
    let script = r#"
import sys
sys.path.insert(0, 'scripts')
from writer_feedback_report import summarize
def record(stage, ns, value=1):
    return {'stage':stage,'pid':1,'id':7,'scope':11,'value':value,'ns':ns}
records = [record('writer.feedback.begin', 10, 0),
           record('writer.feedback.end', 11),
           record('writer.feedback.begin', 12, 1),
           record('writer.feedback.handled', 13, 0),
           record('writer.feedback.end', 14),
           record('writer.feedback.handled', 15, 1)]
row = summarize(records)['clients'][0]
assert row['feedback_pending_high_water'] == 2
assert row['feedback_attempts_while_pending'] == 1
assert row['feedback_unhandled'] == 0
# Send can return after handler receives the event. This is not lost feedback.
assert row['feedback_sent'] == 2
assert row['feedback_requesting_retry'] == 1
assert row['feedback_without_deferred_work'] == 1
records.append(record('writer.feedback.begin', 20, 0))
records.append(record('writer.feedback.end', 21, 0))
assert summarize(records)['clients'][0]['feedback_unhandled'] == 0
"#;
    let status = std::process::Command::new("python3")
        .args(["-c", script])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .expect("writer feedback report executable");
    assert!(status.success());
}
