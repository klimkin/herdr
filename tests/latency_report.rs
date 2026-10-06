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
# Missing capture records cannot prove that this fingerprint was unique.
audit={'processes':[{'pid':1,'complete':True},{'pid':2,'complete':False}]}
assert critical_paths(run,records,audit)==[]
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
fn latency_stage_report_keeps_denominators_and_overlapping_writes() {
    let script = r#"
import sys
sys.path.insert(0, 'scripts')
from latency_report import stage_distributions
run = {'path':'output','client_pids':[2],'clients':1,'samples':[
 {'identity':'7','process_start_ns':100,'observed_ns':[600]},
 {'identity':'8','process_start_ns':100,'observed_ns':[700]},
 {'identity':'9','process_start_ns':100,'observed_ns':[None]}]}
path = {'identity':'7','client_pid':2,'start_ns':100,'terminal_ready_ns':200,
 'surface_ns':300,'enqueue_ns':310,'claim_ns':350,'socket_start_ns':360,
 'socket_complete_ns':420,'client_receive_ns':410,'client_output_ns':500,
 'outer_observed_ns':600,'presentation_gate':{'eligible_ns':250,'frame_start_ns':275}}
report=stage_distributions(run,[path])
assert report['coverage'] == {'offered_effects':3,'completed_effects':2,
 'uniquely_attributed_effects':1,'unassigned_completed_effects':1,
 'ambiguous_completed_effects':0}
stages=report['stages']
assert stages['server_writer_queue']['denominator']==2
assert stages['server_writer_queue']['sample_count']==1
assert stages['server_writer_queue']['p50_ns']==40
assert stages['server_socket_write']['p50_ns']==60
assert stages['write_start_to_client_receive']['p50_ns']==50
assert stages['server_socket_write']['overlaps']==['write_start_to_client_receive','client_work']
assert report['effects'][0]['accounted_pipeline_ns']==500
assert report['effects'][0]['unassigned_pipeline_ns']==0
# Two causal candidates for one completed effect must not double its weight.
ambiguous=stage_distributions(run,[path,{**path,'surface_ns':301}])
assert ambiguous['coverage']['ambiguous_completed_effects']==1
assert ambiguous['stages']['server_writer_queue']['sample_count']==0
assert ambiguous['stages']['server_writer_queue']['p50_ns'] is None
"#;
    let status = std::process::Command::new("python3")
        .args(["-c", script])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .expect("report executable");
    assert!(status.success());
}

#[test]
fn latency_trace_audit_checks_each_expected_process() {
    let script = r#"
import json, sys, tempfile
from pathlib import Path
sys.path.insert(0, 'scripts')
from latency_report import load_traces
run={'server_pid':1,'client_pids':[2,3,4,5],'clients':4}
def record(pid,stage='transport.receive',ns=10,dropped=0):
    return json.dumps({'stage':stage,'pid':pid,'ns':ns,'id':7,'scope':8,'value':9,'dropped':dropped})
with tempfile.TemporaryDirectory() as directory:
    path=Path(directory)
    (path/'1.jsonl').write_text(record(1)+'\n'+record(1,'process.finish',20,3)+'\n')
    (path/'2.jsonl').write_text(record(2)+'\n'+record(2,'process.finish',20,5)+'\n')
    (path/'3.jsonl').write_text(record(3)+'\n'+record(3,'process.finish',20)+'\n'+ '{"stage":')
    (path/'4.jsonl').write_text(record(4)+'\n')
    records,audit=load_traces(run,path)
    processes={item['pid']:item for item in audit['processes']}
    assert processes[1]['finish_marker_present']
    assert processes[1]['dropped_records']==3
    assert processes[3]['truncated'] and processes[3]['invalid_records']==1
    assert not processes[3]['complete']
    assert not processes[4]['finish_marker_present']
    assert not processes[5]['exists']
    assert audit['missing_expected_files']==[5]
    assert audit['total_reported_dropped_records']==8
    assert not audit['all_expected_complete']
    assert all(isinstance(item['ns'],int) for item in records)
    # A clean final marker proves bounded flush completed, even with no events.
    for pid in range(1,6):
        (path/f'{pid}.jsonl').write_text(record(pid,'process.finish',20)+'\n')
    records,audit=load_traces(run,path)
    assert audit['all_expected_complete']
    assert audit['expected_process_count']==5
    # A parseable object without a timestamp cannot reach stage pairing.
    (path/'4.jsonl').write_text('{}\n'+record(4,'process.finish',20)+'\n')
    records,audit=load_traces(run,path)
    assert not audit['all_expected_complete']
    assert len(records)==5
"#;
    let status = std::process::Command::new("python3")
        .args(["-c", script])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .expect("report executable");
    assert!(status.success());
}

#[test]
fn latency_stage_report_counts_completed_effects_without_client_mapping() {
    let script = r#"
import sys
sys.path.insert(0, 'scripts')
from latency_report import stage_distributions
run={'path':'output','clients':2,'client_pids':[2],'samples':[
 {'identity':'7','process_start_ns':100,'observed_ns':[600,700]}]}
report=stage_distributions(run,[])
assert report['coverage']['offered_effects']==2
assert report['coverage']['completed_effects']==2
assert report['coverage']['unassigned_completed_effects']==2
assert report['stages']['client_work']['denominator']==2
assert report['clients'][0]['completed_effects']==1
assert report['clients'][1]['client_pid'] is None
assert report['clients'][1]['completed_effects']==1
"#;
    let status = std::process::Command::new("python3")
        .args(["-c", script])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .expect("report executable");
    assert!(status.success());
}

#[test]
fn latency_stage_report_retains_unassigned_gaps_per_client() {
    let script = r#"
import sys
sys.path.insert(0,'scripts')
from latency_report import stage_distributions
run={'path':'output','clients':2,'client_pids':[2,3],'samples':[
 {'identity':'7','process_start_ns':100,'observed_ns':[600,900]}]}
paths=[{'identity':'7','client_pid':2,'start_ns':100,'terminal_ready_ns':200,
 'enqueue_ns':310,'claim_ns':350,'client_receive_ns':410,'client_output_ns':500,'outer_observed_ns':600}]
report=stage_distributions(run,paths)
assert report['effects'][0]['unassigned_intervals']==[{'start_ns':200,'end_ns':310,'duration_ns':110},
 {'start_ns':350,'end_ns':410,'duration_ns':60}]
assert report['effects'][0]['unassigned_pipeline_ns']==170
assert report['clients'][0]['stages']['server_writer_queue']['p50_ns']==40
assert report['clients'][1]['stages']['server_writer_queue']['sample_count']==0
assert report['clients'][1]['stages']['server_writer_queue']['denominator']==1
"#;
    let status = std::process::Command::new("python3")
        .args(["-c", script])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .expect("report executable");
    assert!(status.success());
}

#[test]
fn latency_report_cli_publishes_stage_coverage_and_trace_completeness() {
    let script = r#"
import json, subprocess, sys, tempfile
from pathlib import Path
run={'path':'output','load':'quiet','panes':1,'clients':1,'server_pid':1,'client_pids':[2],
 'budget_ns':1000,'summary':[{'completed':1,'missing':0,'p50_ns':500,'p95_ns':500,'p99_ns':500}],
 'samples':[{'identity':'7','process_start_ns':100,'observed_ns':[600]}]}
def record(pid,stage,ns,id=0,scope=0,value=0):
    return json.dumps(dict(pid=pid,stage=stage,ns=ns,id=id,scope=scope,value=value,dropped=0))
with tempfile.TemporaryDirectory() as directory:
    path=Path(directory); traces=path/'traces'; traces.mkdir()
    (path/'samples.json').write_text(json.dumps(run))
    (traces/'1.jsonl').write_text(record(1,'process.finish',700)+'\n')
    subprocess.run([sys.executable,'scripts/latency_report.py',str(path/'samples.json'),
       '--traces',str(traces),'--output',str(path/'report.md')],check=True)
    report=json.loads((path/'report.json').read_text())
    assert report['stage_report']['coverage']['completed_effects']==1
    assert report['stage_report']['stages']['client_work']['sample_count']==0
    assert report['trace_audit']['missing_expected_files']==[2]
    text=(path/'report.md').read_text()
    assert 'Stage | Matched / completed' in text
    assert 'Finish | Drops | Integrity' in text
    assert 'per-stage percentiles are not additive' in text
    # Reports without diagnostic captures still preserve coverage denominators.
    subprocess.run([sys.executable,'scripts/latency_report.py',str(path/'samples.json'),
       '--output',str(path/'untraced.md')],check=True)
    report=json.loads((path/'untraced.json').read_text())
    assert report['stage_report']['coverage']['completed_effects']==1
    assert report['trace_audit'] is None
"#;
    let status = std::process::Command::new("python3")
        .args(["-c", script])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .expect("report executable");
    assert!(status.success());
}

#[test]
fn latency_trace_audit_reports_observed_flush_failures() {
    let script = r#"
import json, sys, tempfile
from pathlib import Path
sys.path.insert(0,'scripts')
from latency_report import load_traces
run={'server_pid':1,'client_pids':[2],'clients':1}
with tempfile.TemporaryDirectory() as directory:
    path=Path(directory)/'traces'; path.mkdir()
    for pid in (1,2):
        (path/f'{pid}.jsonl').write_text(json.dumps({'stage':'process.finish','pid':pid,
          'ns':20,'id':0,'scope':0,'value':0,'dropped':0})+'\n')
    (path.parent/'server.stderr').write_text('latency recorder final flush timed out; trailing records may be missing\n')
    (path.parent/'client-0.vt').write_bytes(b'\x1b[31mlatency recorder final flush unavailable\r\n')
    _,audit=load_traces(run,path)
    assert not audit['all_expected_complete']
    assert all('observed bounded-flush failure' in p['issues'] for p in audit['processes'])
    assert audit['processes'][0]['flush_failures']==['latency recorder final flush timed out']
    assert audit['processes'][1]['flush_failures']==['latency recorder final flush unavailable']
"#;
    let status = std::process::Command::new("python3")
        .args(["-c", script])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .expect("report executable");
    assert!(status.success());
}

#[test]
fn latency_trace_audit_rejects_ambiguous_processes_and_finish_order() {
    let script = r#"
import json, sys, tempfile
from pathlib import Path
sys.path.insert(0,'scripts')
from latency_report import load_traces
def record(pid,stage,ns):
    return json.dumps(dict(pid=pid,stage=stage,ns=ns,id=0,scope=0,value=0,dropped=0))
with tempfile.TemporaryDirectory() as directory:
    path=Path(directory)
    (path/'1.jsonl').write_text(record(1,'transport.receive',30)+'\n'+record(1,'process.finish',20)+'\n')
    (path/'2.jsonl').write_text(record(2,'process.finish',20)+'\n')
    _,audit=load_traces({'server_pid':1,'client_pids':[2,2],'clients':2},path)
    assert not audit['expected_process_metadata_complete']
    assert not audit['all_expected_complete']
    assert 'event timestamp after finish marker' in audit['processes'][0]['issues']
    _,audit=load_traces({'server_pid':1,'client_pids':[None,2],'clients':2},path)
    assert not audit['expected_process_metadata_complete']
    assert not audit['all_expected_complete']
"#;
    let status = std::process::Command::new("python3")
        .args(["-c", script])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .expect("report executable");
    assert!(status.success());
}
