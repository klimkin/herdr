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
records += [{'stage':'process.finish','pid':pid,'id':0,'scope':0,'value':0,'ns':1100,'dropped':0} for pid in (1,2,3)]
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
records.append({'stage':'server.connection','pid':1,'id':7,'scope':12,'value':2,'ns':25})
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
