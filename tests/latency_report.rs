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

#[test]
fn latency_trace_report_pairs_fanout_by_connection_and_wire_position() {
    let script = r#"
import sys
sys.path.insert(0, 'scripts')
from latency_report import critical_paths, pair_stages
run={'path':'action','client_pids':[2,3], 'samples':[{'identity':'000000000007','injected_ns':100,'observed_ns':[800,1000]}]}
records=[
 {'stage':'server.connection','pid':1,'id':5,'scope':10,'value':2,'ns':10},
 {'stage':'server.connection','pid':1,'id':6,'scope':11,'value':3,'ns':10},
 {'stage':'client.connection','pid':2,'id':20,'scope':0,'value':1,'ns':20},
 {'stage':'client.connection','pid':3,'id':30,'scope':0,'value':1,'ns':20},
 {'stage':'snapshot.label','pid':1,'id':None,'scope':777,'value':1,'ns':200},
]
from latency_report import fingerprint
records[-1]['id']=fingerprint('A000007')
def add(stage,pid,scope,connection,occurrence,offset,ns):
 records.append({'stage':stage,'pid':pid,'scope':scope,'connection':connection,'occurrence':occurrence,'offset':offset,'id':777,'value':20,'ns':ns})
for scope,conn,occ,offset,start in [(10,5,101,0,210),(12,6,102,0,220),(10,5,103,24,230)]:
 add('server.enqueue',1,scope,conn,occ,0,start)
 add('server.writer_claim',1,scope,conn,occ,offset,start+10)
 add('server.write_start',1,scope,conn,occ,offset,start+11)
 add('server.write_complete',1,scope,conn,occ,offset,start+20)
add('transport.receive',2,0,20,0,0,300)
add('client.delivery',2,0,20,0,0,500)
add('transport.receive',2,0,20,0,24,320)
add('client.delivery',2,0,20,0,24,600)
add('transport.receive',3,0,30,0,0,400)
add('client.delivery',3,0,30,0,0,900)
# Recorder finish follows every captured event for that process.
records += [{'stage':'process.finish','pid':pid,'id':0,'scope':0,'value':0,'ns':1100,'dropped':0} for pid in (1,2,3)]
paths=critical_paths(run,records)
assert sorted((p['client_pid'],p['occurrence']) for p in paths)==[(2,101),(3,102)],paths
assert all(p['connection_attribution']=='native peer PID and framed-byte offset' for p in paths)
assert all(p['socket_start_ns'] < p['socket_complete_ns'] for p in paths)
audit={'processes':[{'pid':pid,'complete':True} for pid in (1,2,3)]}
assert len(critical_paths(run,records,audit))==2
audit['processes'][2]['complete']=False
assert [p['client_pid'] for p in critical_paths(run,records,audit)]==[2]
# Missing enqueue never shifts the next identical frame onto the earlier claim.
lost=[r for r in records if not (r['stage']=='server.enqueue' and r.get('occurrence')==101)]
paths=critical_paths(run,lost)
assert sorted((p['client_pid'],p['occurrence']) for p in paths)==[(2,103),(3,102)],paths
# Missing receive never lets a subsequent receive claim an earlier presentation.
lost=[r for r in records if not (r['stage']=='transport.receive' and r['pid']==2 and r.get('offset')==0)]
paths=critical_paths(run,lost)
assert sorted((p['client_pid'],p['occurrence']) for p in paths)==[(2,103),(3,102)],paths
# Another connection from the same PID makes native PID mapping insufficient.
records.insert(-3, {'stage':'server.connection','pid':1,'id':7,'scope':12,'value':2,'ns':25})
assert [p['client_pid'] for p in critical_paths(run,records)]==[3]
# Missing final flush or recorder loss invalidates connection completeness.
assert critical_paths(run,[r for r in records if not(r['stage']=='process.finish' and r['pid']==3)])==[]
server_finish=next(r for r in records if r['stage']=='process.finish' and r['pid']==1)
server_finish['dropped']=1
assert critical_paths(run,records)==[]
server_finish['dropped']=0
# Occurrence joins do not fabricate enqueue after discard or record loss.
assert len(pair_stages(records))==3
lost=[r for r in records if not (r['stage']=='server.enqueue' and r.get('occurrence')==101)]
assert sorted(p['occurrence'] for p in pair_stages(lost))==[102,103]
# Render discard clears only the matching lane; exact identities still distinguish repeats.
discard={'stage':'server.queue_discard','pid':1,'scope':10,'id':0,'ns':235}
assert sorted(p['occurrence'] for p in pair_stages(records+[discard]))==[101,102]
# A missing destination stays unavailable rather than inheriting its peer's presentation.
run['samples'][0]['observed_ns']=[1000,None]
assert [p['client_pid'] for p in critical_paths(run,records)]==[]
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

#[test]
fn latency_echo_report_joins_actor_acceptance_to_helper_receipt() {
    let script = r#"
import sys
sys.path.insert(0,'scripts')
from latency_report import critical_paths
run={'path':'echo','clock':'host monotonic; observer includes scheduling and reconstruction',
 'clock_validation_ns':[10,15,20],'client_pids':[2],'samples':[{
 'identity':'7','injected_ns':100,'helper_received_ns':205,'observed_ns':[600]}]}
def event(stage,ns,id=7,scope=80,value=1,pid=1):
 return dict(stage=stage,ns=ns,id=id,scope=scope,value=value,pid=pid,dropped=0)
records=[event('input.actor_enqueue',120,id=1),event('input.actor_claim',150,id=1),
 event('input.actor_part',151,id=1),event('input.actor_fragment',119,value=1),
 event('input.pty_pending',160,id=1),event('input.pty_write_attempt',170,id=1),
 event('input.pty_part_complete',190,id=1),event('terminal.stimulus',220,scope=2,value=4),
 event('surface.content',300,id=2,scope=777,value=6),
 event('server.enqueue',310,id=777,scope=10),event('server.writer_claim',350,id=777,scope=10),
 event('server.write_complete',400,id=777,scope=10),event('transport.receive',410,id=777,pid=2),
 event('client.delivery',500,id=777,pid=2),event('process.finish',650,id=0,scope=0),
 event('process.finish',650,id=0,scope=0,pid=2)]
audit={'processes':[{'pid':1,'complete':True},{'pid':2,'complete':True}]}
path=critical_paths(run,records,audit)[0]
assert path['actor_input']['attribution']=='complete contributing accepted parts'
assert path['helper_received_ns']==205
assert path['actor_input']['write_complete_to_helper_received_ns']==15
assert 'PTY submission queue residence' not in path['unavailable']
# Missing trace completion cannot establish all contributing fragments.
unfinished=[record for record in records if record['stage']!='process.finish']
path=critical_paths(run,unfinished)[0]
assert path['actor_input']['attribution']=='incomplete trace for contributing parts'
assert 'PTY submission queue residence' in path['unavailable']
# Consumption can precede the observed write return; never clamp to zero.
run['samples'][0]['helper_received_ns']=180
path=critical_paths(run,records,audit)[0]
assert path['actor_input']['write_complete_to_helper_received_ns'] is None
run['clock_validation_ns']=[10,20]
path=critical_paths(run,records,audit)[0]
assert path['actor_input']['helper_clock_verified'] is False
assert path['helper_received_ns'] is None
"#;
    let status = std::process::Command::new("python3")
        .args(["-c", script])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .expect("report executable");
    assert!(status.success());
}

#[test]
fn latency_input_distributions_count_commands_once_and_preserve_gaps() {
    let script = r#"
import sys
sys.path.insert(0,'scripts')
from latency_report import input_distributions
run={'path':'echo','samples':[{'identity':'7','observed_ns':[600,700]},
                            {'identity':'8','observed_ns':[800,900]}]}
part={'scope':80,'command_id':1,'part_id':1,'command_queue_ns':30,
      'claim_to_pending_ns':10,'pending_to_attempt_ns':10,'attempt_to_complete_ns':20}
actor={'attribution':'complete contributing accepted parts','trace_complete':True,
       'parts':[part,{**part,'part_id':2,'pending_to_attempt_ns':15}],
       'write_complete_to_helper_received_ns':5}
paths=[{'identity':'7','server_pid':1,'client_pid':2,'actor_input':actor},
       {'identity':'7','server_pid':1,'client_pid':3,'actor_input':actor}]
report=input_distributions(run,paths)
assert report['coverage']=={'completed_echo_probes':2,'matched_actor_probes':1,'unassigned_echo_probes':1}
assert report['stages']['command_queue']['sample_count']==1
assert report['stages']['command_queue']['matched_probes']==1
assert report['stages']['command_queue']['probe_denominator']==2
assert report['stages']['command_queue']['p50_ns']==30
assert report['stages']['pending_to_attempt']['sample_count']==2
assert report['stages']['pending_to_attempt']['p95_ns']==15
assert report['stages']['write_complete_to_helper_received']['p50_ns']==5
# Incomplete captures keep every probe denominator but establish no parts.
paths[0]['actor_input']={**actor,'trace_complete':False}
paths[1]['actor_input']=paths[0]['actor_input']
report=input_distributions(run,paths)
assert report['coverage']['matched_actor_probes']==0
assert report['stages']['command_queue']['sample_count']==0
assert report['stages']['command_queue']['probe_denominator']==2
"#;
    let status = std::process::Command::new("python3")
        .args(["-c", script])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .expect("report executable");
    assert!(status.success());
}

#[test]
fn latency_echo_cli_reports_input_units_without_fabricating_missing_stages() {
    let script = r#"
import json, subprocess, sys, tempfile
from pathlib import Path
run={'path':'echo','load':'quiet','panes':1,'clients':1,'budget_ns':1000,
     'summary':[{'completed':1,'missing':0,'p50_ns':500,'p95_ns':500,'p99_ns':500}],
     'samples':[{'identity':'7','injected_ns':100,'observed_ns':[600]}]}
with tempfile.TemporaryDirectory() as directory:
    path=Path(directory)
    (path/'samples.json').write_text(json.dumps(run))
    subprocess.run([sys.executable,'scripts/latency_report.py',str(path/'samples.json'),
      '--output',str(path/'report.md')],check=True)
    report=json.loads((path/'report.json').read_text())
    assert report['input_report']['coverage']['completed_echo_probes']==1
    assert report['input_report']['stages']['command_queue']['sample_count']==0
    assert report['input_report']['stages']['command_queue']['probe_denominator']==1
    assert 'Input stage | Matched probes / completed' in (path/'report.md').read_text()
"#;
    let status = std::process::Command::new("python3")
        .args(["-c", script])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .expect("report executable");
    assert!(status.success());
}

#[test]
fn latency_reader_recovery_keeps_fixed_cohort_and_missing_boundaries() {
    let script = r#"
import sys
sys.path.insert(0, 'scripts')
from latency_report import reader_recovery
lifecycle = {'client_index':1,'client_pid':22,'requested_ms':1000,
 'installed_ns':80,'paused_ns':90,'reset_request_ns':200,'reset_complete_ns':205,
 'reader_resumed_ns':210,'first_read_attempt_ns':212,'first_receipt_ns':215,'stopped':False}
run = {'clients':2,'client_pids':[21,22],'stall_ms':1000,'slow_reader_ms':0,
 'reader_lifecycle':lifecycle,'finished_ns':500,'clock_validation_ns':[10,20,30],
 'warmups':1,'offered_samples':3,
 'recovery_warmups':[{'identity':'00','injected_ns':95,'accepted_ns':100,'observed_ns':[120,250],'outcome':'presented'}],
 'samples':[{'identity':'01','injected_ns':140,'accepted_ns':150,'observed_ns':[170,300],'outcome':'presented'},
 {'identity':'02','injected_ns':160,'accepted_ns':None,'observed_ns':[None,None],'outcome':'rejected: full'},
 {'identity':'03','injected_ns':220,'accepted_ns':225,'observed_ns':[240,400],'outcome':'presented'}]}
result=reader_recovery(run)
assert result['status']=='measured'
assert result['accepted_cutoff_identity']=='01'
assert result['cohort_accepted']==2
assert result['rejected_before_reset']==1
assert result['clients'][1]['backlog_at_reset']==2
assert result['clients'][1]['oldest_pending_age_at_reset_ns']==105
assert result['clients'][1]['catch_up_ns']==300
assert result['clients'][1]['reset_to_catch_up_bounds_ns']==[95,100]
assert result['clients'][1]['reader_resume_to_catch_up_ns']==90
assert result['clients'][0]['status']=='already_caught_up'
# Later arrivals do not enlarge the fixed accepted cohort.
run['samples'][2]['observed_ns'][1]=None
assert reader_recovery(run)['clients'][1]['catch_up_ns']==300
run['samples'][0]['observed_ns'][1]=None
missing=reader_recovery(run)['clients'][1]
assert missing['status']=='incomplete'
assert missing['missing']==1 and missing['catch_up_ns'] is None
# Controller reset and earlier in-flight receipt cannot become negative/zero resume latency.
run['samples'][0]['observed_ns'][1]=202
run['recovery_warmups'][0]['observed_ns'][1]=201
early=reader_recovery(run)['clients'][1]
assert early['catch_up_ns']==202
assert early['reset_to_catch_up_bounds_ns'] is None
assert early['reader_resume_to_catch_up_ns'] is None
assert early['ordering']=='receipt precedes reset completion or reader resume'
# Old archives never substitute started+requested delay for reset.
old={**run,'started_ns':0};del old['reader_lifecycle']
assert reader_recovery(old)['status']=='unavailable'
assert reader_recovery({**old,'stall_ms':0,'slow_reader_ms':120})['status']=='not_applicable'
assert reader_recovery({**run,'reader_lifecycle':{**lifecycle,'paused_ns':None}})['status']=='unavailable'
assert reader_recovery({**run,'client_pids':[21,999]})['status']=='unavailable'
# No accepted cohort and all-rejected probes retain counts rather than zero recovery.
empty={**run,'recovery_warmups':[],'samples':run['samples'][1:2],'warmups':0,'offered_samples':1}
none=reader_recovery(empty)
assert none['cohort_accepted']==0 and none['rejected_before_reset']==1
assert none['clients'][1]['status']=='no_backlog'
assert none['clients'][1]['catch_up_ns'] is None
assert none['clients'][1]['reset_to_catch_up_bounds_ns'] is None
# Unknown interrupted completion and inconsistent membership remain unassigned.
unknown={**run,'samples':[{**run['samples'][1],'outcome':'interrupted: timeout'}],'offered_samples':1}
assert reader_recovery(unknown)['status']=='unavailable'
assert reader_recovery(unknown)['unknown_submissions']==1
invalid={**run,'reader_lifecycle':{**lifecycle,'reset_complete_ns':211}}
assert reader_recovery(invalid)['status']=='unavailable'
assert reader_recovery({**run,'client_pids':None})['status']=='unavailable'
assert reader_recovery({**run,'clock_validation_ns':[10,30,20]})['status']=='unavailable'
assert reader_recovery({**run,'samples':[{**run['samples'][0],'observed_ns':[None]}]})['status']=='unavailable'
assert reader_recovery({**run,'samples':[None]})['status']=='unavailable'
assert reader_recovery({**run,'recovery_warmups':[]})['status']=='unavailable'
assert reader_recovery({**run,'samples':run['samples'][:-1]})['status']=='unavailable'


"#;
    let status = std::process::Command::new("python3")
        .args(["-c", script])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .expect("report executable");
    assert!(status.success());
}
