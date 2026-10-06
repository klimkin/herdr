#[path = "../examples/latency/report.rs"]
mod report;

#[test]
fn latency_report_keeps_missing_outcomes_and_sparse_tails() {
    let summary = report::summarize(&[1, 2, 3, 4, 100], 7, 10);
    assert_eq!(summary.completed, 5);
    assert_eq!(summary.missing, 2);
    assert_eq!(summary.p50_ns, Some(3));
    assert_eq!(summary.p95_ns, Some(100));
    assert_eq!(summary.p999_ns, None);
    assert_eq!(summary.deadline_misses, 3);
}

#[test]
fn latency_trace_report_keeps_client_queues_distinct() {
    let script = r#"
import sys
sys.path.insert(0, 'scripts')
from latency_report import pair_stages
records = [
    {'stage':'server.enqueue', 'pid':1, 'scope':10, 'id':7, 'ns':100},
    {'stage':'server.enqueue', 'pid':1, 'scope':20, 'id':7, 'ns':200},
    {'stage':'server.writer_claim', 'pid':1, 'scope':20, 'id':7, 'ns':210},
    {'stage':'server.writer_claim', 'pid':1, 'scope':10, 'id':7, 'ns':500},
]
assert sorted(pair['queue_ns'] for pair in pair_stages(records)) == [10, 400]
# A discarded frame cannot become the next identical frame's enqueue.
records += [{'stage':'server.enqueue','pid':1,'scope':30,'id':7,'ns':600},
            {'stage':'server.queue_discard','pid':1,'scope':30,'id':0,'ns':700},
            {'stage':'server.enqueue','pid':1,'scope':30,'id':7,'ns':800},
            {'stage':'server.writer_claim','pid':1,'scope':30,'id':7,'ns':900}]
assert sorted(pair['queue_ns'] for pair in pair_stages(records)) == [10,100,400]
# Old records cannot establish connection identity.
for record in records:
    del record['scope']
assert pair_stages(records) == []
"#;
    let status = std::process::Command::new("python3")
        .args(["-c", script])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .expect("report executable");
    assert!(status.success());
}

#[test]
fn latency_trace_report_joins_only_proven_deliveries() {
    let script = r#"
import sys
sys.path.insert(0, 'scripts')
from latency_report import critical_paths
run = {'path':'output','client_pids':[2],'samples':[{'identity':'000000000007','injected_ns':90,'process_start_ns':100,'observed_ns':[600]}]}
records = [
 {'stage':'terminal.stimulus','id':7,'scope':2,'value':4,'ns':200,'pid':1},
 {'stage':'surface.content','id':2,'scope':777,'value':6,'ns':300,'pid':1},
 {'stage':'server.enqueue','id':777,'scope':10,'value':20,'ns':310,'pid':1},
 {'stage':'server.writer_claim','id':777,'scope':10,'value':20,'ns':350,'pid':1},
 {'stage':'server.write_complete','id':777,'scope':10,'value':20,'ns':400,'pid':1},
 {'stage':'transport.receive','id':777,'scope':0,'value':20,'ns':410,'pid':2},
 {'stage':'client.delivery','id':777,'scope':0,'value':20,'ns':500,'pid':2},
]
records += [
 {'stage':'presentation.selected_deadline','id':0,'scope':0,'value':150,'ns':250,'pid':1},
 {'stage':'presentation.selected_remaining','id':0,'scope':0,'value':0,'ns':250,'pid':1},
 {'stage':'presentation.selected_overdue','id':0,'scope':0,'value':100,'ns':250,'pid':1},
 {'stage':'server.frame_start','id':0,'scope':0,'value':0,'ns':275,'pid':1},
]
paths=critical_paths(run,records)
assert len(paths)==1
assert paths[0]['presentation_gate']['eligible_ns']==200
assert paths[0]['presentation_gate']['eligible_to_frame_start_ns']==75
assert paths[0]['terminal_revision']==4
assert paths[0]['surface_content_revision']==6
assert paths[0]['queue_ns']==40
assert paths[0]['client_output_ns']==500
assert paths[0]['outer_observed_ns']==600
# Two stimuli at one content revision cannot both prove a replaced row.
records.append({'stage':'terminal.stimulus','id':8,'scope':2,'value':4,'ns':201,'pid':1})
assert critical_paths(run,records)==[]
# No frame identity means no fabricated stage attribution.
assert critical_paths(run,records[:1])==[]
"#;
    let status = std::process::Command::new("python3")
        .args(["-c", script])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .expect("report executable");
    assert!(status.success());
}

#[test]
fn latency_report_keeps_actor_parts_and_missing_queue_boundaries() {
    let script = r#"
import sys
sys.path.insert(0, 'scripts')
from latency_report import input_actor_path
records = [
 {'stage':'input.actor_enqueue','pid':1,'scope':80,'id':1,'value':7,'ns':100},
 {'stage':'input.actor_claim','pid':1,'scope':80,'id':1,'value':7,'ns':200},
 {'stage':'input.actor_part','pid':1,'scope':80,'id':1,'value':1,'ns':205},
 {'stage':'input.actor_fragment','pid':1,'scope':80,'id':7,'value':1,'ns':310},
 {'stage':'input.pty_pending','pid':1,'scope':80,'id':1,'value':7,'ns':210},
 {'stage':'input.pty_write_attempt','pid':1,'scope':80,'id':1,'value':7,'ns':220},
 {'stage':'input.pty_part_complete','pid':1,'scope':80,'id':1,'value':7,'ns':250},
 {'stage':'input.actor_enqueue','pid':1,'scope':80,'id':2,'value':7,'ns':300},
 {'stage':'input.actor_claim','pid':1,'scope':80,'id':2,'value':7,'ns':400},
 {'stage':'input.actor_part','pid':1,'scope':80,'id':2,'value':2,'ns':405},
 {'stage':'input.actor_fragment','pid':1,'scope':80,'id':7,'value':2,'ns':410},
 {'stage':'input.pty_pending','pid':1,'scope':80,'id':2,'value':7,'ns':420},
 {'stage':'input.pty_write_attempt','pid':1,'scope':80,'id':2,'value':7,'ns':430},
 {'stage':'input.pty_part_complete','pid':1,'scope':80,'id':2,'value':7,'ns':900},
]
path=input_actor_path(records,7,1,90,1000)
assert path['attribution']=='complete contributing accepted parts'
assert [part['command_queue_ns'] for part in path['parts']]==[100,100]
assert [part['claim_to_pending_ns'] for part in path['parts']]==[10,20]
assert [part['pending_to_attempt_ns'] for part in path['parts']]==[10,10]
assert [part['attempt_to_complete_ns'] for part in path['parts']]==[30,470]
assert path['first_enqueue_ns']==100
assert path['final_write_complete_ns']==900
# No accepted record, no fabricated residence.
missing=[record for record in records if not (record['stage']=='input.actor_enqueue' and record['id']==2)]
path=input_actor_path(missing,7,1,90,1000)
assert path['attribution']=='incomplete contributing parts'
assert path['parts'][1]['command_queue_ns'] is None
# Discard is terminal, even when a conflicting complete record appears later.
records.append({'stage':'input.pty_part_discard','pid':1,'scope':80,'id':2,'value':7,'ns':850})
path=input_actor_path(records,7,1,90,1000)
assert path['parts'][1]['attempt_to_complete_ns'] is None
assert path['final_write_complete_ns'] is None
# Repeated identity in distinct commands cannot prove one controlled submission.
records += [dict(record, scope=81, ns=record['ns']+1) for record in records[:7]]
assert input_actor_path(records,7,1,90,1000)['attribution']=='ambiguous actor identity'
assert input_actor_path([],7,1,90,1000)['attribution']=='actor records unavailable'
"#;
    let status = std::process::Command::new("python3")
        .args(["-c", script])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .expect("report executable");
    assert!(status.success());
}
