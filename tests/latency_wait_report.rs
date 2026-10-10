#[test]
fn latency_report_leaves_unarmed_and_incomplete_waits_unassigned() {
    let script = r#"
import sys
sys.path.insert(0, 'scripts')
from latency_report import wait_service

def record(stage, identifier, ns, deadline, mask):
    return {'stage':stage, 'pid':1, 'id':identifier, 'ns':ns,
            'value':deadline, 'scope':mask}

records = []
for identifier, deadline, mask in [(1,150,4352), (2,0,4096), (3,150,0),
                                  (4,150,32768), (5,150,4096)]:
    records.append(record('loop.wait_begin',identifier,100,deadline,mask))
    records.append(record('loop.wait_end.deadline',identifier,170,deadline,mask))
# Contradictory metadata cannot describe the armed future.
records[-1]['scope'] = 256
# A selected return with no begin, a begin with no return, and duplicate returns
# each remain unresolved instead of disappearing from the invalid denominator.
records.append(record('loop.wait_end.render',6,170,0,0))
records.append(record('loop.wait_begin',7,100,0,0))
records.append(record('loop.wait_begin',8,100,0,0))
records += [record('loop.wait_end.render',8,170,0,0)] * 2

report = wait_service(records)
assert report['invalid_waits'] == 7, report
assert report['deadline_returns'] == [{'pid':1,'wait':1,'armed_deadline_ns':150,
    'reasons_mask':4352,'return_ns':170,'overdue_ns':20}], report
assert report['deadline_reason_names'] == {
    '4352':['agent_metadata','render_cadence']}, report
assert report['scheduler_wakeups'] is None
assert report['context_switches'] is None
"#;
    let output = std::process::Command::new("python3")
        .args(["-c", script])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run public wait report");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
