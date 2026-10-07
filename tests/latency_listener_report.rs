#[test]
fn latency_listener_report_keeps_outer_completion_without_ambiguous_connection_attribution() {
    let script = r#"
import sys
sys.path.insert(0, 'scripts')
from latency_listener_report import listener_report

def record(stage, pid, identifier, ns, value=0, scope=0):
    return {'stage':stage, 'pid':pid, 'id':identifier, 'ns':ns,
            'value':value, 'scope':scope, 'dropped':0}

records = [record('diagnostics.schema',pid,0,1,2) for pid in (1,2)]
records += [record('client.connect_begin',2,7,10),
            record('client.connect_complete',2,7,22,1),
            record('client.accepted',1,8,20,2),
            record('client.handshake_begin',1,8,25),
            record('client.hello_ready',1,8,30),
            record('client.welcome_written',1,8,35),
            record('client.hello_begin',2,9,23),
            record('client.hello_written',2,9,26),
            record('client.welcome_received',2,9,38),
            record('server.connection',1,11,39,2,8),
            record('client.connection',2,12,40,1),
            record('client.output_complete',2,13,45,32)]
records += [record('process.finish',pid,0,100) for pid in (1,2)]
run = {'server_pid':1, 'client_pids':[2], 'attaches':[
    {'identity':'a','client_pid':2,'intended_ns':5,'launch_ns':8,
     'observed_ns':50,'first_receipt_ns':46}]}
report = listener_report(run,records)
row = report['attaches'][0]
assert row['status'] == 'measured', row
assert row['connect_to_accept_ns'] == 10, row
assert row['launch_to_observed_ns'] == 42, row
assert report['summary']['connect_to_accept']['sample_count'] == 1
# Another socket attempt by the same process invalidates the internal mapping;
# authoritative external completion remains measurable.
records.insert(-1,record('client.connect_begin',2,17,60))
ambiguous = listener_report(run,records)
assert ambiguous['attaches'][0]['status'] == 'unassigned', ambiguous
assert ambiguous['attaches'][0]['connect_to_accept_ns'] is None
assert ambiguous['attaches'][0]['launch_to_observed_ns'] == 42
assert ambiguous['summary']['connect_to_accept']['sample_count'] == 0
assert ambiguous['summary']['launch_to_observed']['sample_count'] == 1
# Failed/retried handshakes and invalid identities never yield a partial causal claim.
clean = [item for item in records if item['id'] != 17]
for bad in [record('client.hello_failed',2,9,27),
            record('client.connection',2,22,41,1),
            record('client.accepted',1,8,20,2),
            record('client.welcome_received',2,19,39),
            record('client.hello_begin',2,19,39),
            record('client.connect_complete',2,27,23,1),
            record('client.hello_written',2,29,27)]:
    trial = clean[:-2] + [bad] + clean[-2:]
    result = listener_report(run,trial)
    assert result['summary']['connect_to_accept']['sample_count'] == 0, (bad,result)
    assert result['summary']['launch_to_observed']['sample_count'] == 1
for field, value in [('pid',True),('pid',-1),('ns',False),('ns',-3),('id',None)]:
    trial = [dict(item) for item in clean]
    trial[2][field] = value
    result = listener_report(run,trial)
    assert result['attaches'][0]['status'] == 'unassigned', (field,value,result)
# Outer observation can precede flush return; accept remains proven, observer gap unavailable.
late = [dict(item) for item in clean]
for item in late:
    if item['stage'] == 'client.output_complete':
        item['ns'] = 51
result = listener_report(run,late)
assert result['attaches'][0]['status'] == 'measured'
assert result['attaches'][0]['output_to_observed_ns'] is None
assert result['attaches'][0]['launch_to_observed_ns'] == 42
# Peer receipt can precede server write-return recording. Exact accept stays proven.
overlap = [dict(item) for item in clean]
for item in overlap:
    if item['stage'] == 'client.welcome_written':
        item['ns'] = 39
result = listener_report(run,overlap)
assert result['attaches'][0]['status'] == 'measured', result
assert result['attaches'][0]['connect_to_accept_ns'] == 10
assert result['attaches'][0]['welcome_return_to_received_ns'] is None
# Zero IDs and lifetime aliases cannot prove an exact connection.
for stage in ('client.connect_begin','client.accepted','server.connection','client.connection'):
    trial = [dict(item) for item in clean]
    for item in trial:
        if item['stage'] == stage:
            item['id'] = 0
    result = listener_report(run,trial)
    assert result['attaches'][0]['status'] == 'unassigned', (stage,result)
for pids in ([2,2],[1,2]):
    result = listener_report({**run,'client_pids':pids},clean)
    assert result['attaches'][0]['status'] == 'unassigned', result
trial = clean[:-2] + [record('server.connection',1,77,41,2,78)] + clean[-2:]
assert listener_report(run,trial)['attaches'][0]['status'] == 'unassigned'
# A marker observation and incomplete raw file must retain the timing but fail fidelity.
raw_run = {**run, 'artifact_status':[{'observer_pid':2,'observer_bytes':6,
    'raw_file_valid':True,'read_complete':True,'reader_error':None,'output_index':0,'raw_file':'client-0.vt','sink':{'complete':True,'accepted_bytes':6,'persisted_bytes':6,
                               'dropped_batches':0,'error':None}}]}
assert listener_report(raw_run,clean)['population']['validated_outer_completed'] == 1
raw_run['artifact_status'][0]['sink']['complete'] = False
result = listener_report(raw_run,clean)
assert result['population']['outer_completed'] == 1
assert result['population']['validated_outer_completed'] == 0
assert result['attaches'][0]['launch_to_observed_ns'] == 42
# Schema and lifetime boundaries are part of measurement fidelity.
for bad_records in ([item for item in clean if not (item['stage']=='diagnostics.schema' and item['pid']==2)],
                    clean[:-2]+[record('diagnostics.schema',2,0,1,2)]+clean[-2:]):
    assert listener_report(run,bad_records)['attaches'][0]['status'] == 'unassigned'
late_launch = {**run,'attaches':[{**run['attaches'][0],'launch_ns':11}]}
assert listener_report(late_launch,clean)['attaches'][0]['status'] == 'unassigned'
raw_run['artifact_status'][0]['sink']['complete'] = True
raw_run['artifact_status'][0]['reader_error'] = 'EOF not proven'
assert listener_report(raw_run,clean)['population']['validated_outer_completed'] == 0
raw_run['artifact_status'][0]['reader_error'] = None
raw_run['artifact_status'][0]['output_index'] = 1
assert listener_report(raw_run,clean)['population']['validated_outer_completed'] == 0
raw_run['artifact_status'][0]['output_index'] = 0
raw_run['artifact_status'][0].pop('raw_file_valid')
assert listener_report(raw_run,clean)['population']['validated_outer_completed'] == 0
# Malformed lifecycle metadata cannot make artifact index lookup throw.
assert listener_report({**raw_run,'client_pids':[]},clean)['population']['validated_outer_completed'] == 0
"#;
    let output = std::process::Command::new("python3")
        .args(["-c", script])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run public listener report");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn latency_listener_process_output_report_uses_helper_write_start() {
    let script = r#"
import sys
sys.path.insert(0,'scripts')
from latency_listener_report import process_output_report
row={'identity':'p','helper_reply_identity':'p','kind':'output','submitted_ns':10,'process_write_start_ns':50,
     'process_write_end_ns':55,'observed_ns':70,'clock_bracket_ns':[10,60,80]}
r=process_output_report([row])
assert r['responses'][0]['process_to_observed_ns']==20, r
assert r['responses'][0]['controller_to_observed_ns']==60, r
missing={**row,'process_write_start_ns':None}
r=process_output_report([missing])
assert r['responses'][0]['process_to_observed_ns'] is None, r
late={**row,'process_write_end_ns':71}
r=process_output_report([late])
assert r['responses'][0]['process_to_observed_ns']==20, r
assert r['responses'][0]['write_return_to_observed_ns'] is None, r
for changed in ({**row,'identity':''},{**row,'clock_bracket_ns':None},
                {**row,'process_write_start_ns':True},{**row,'process_write_start_ns':1<<65},{**row,'helper_reply_identity':'wrong'},{**row,'observed_ns':90},{**row,'submitted_ns':9}):
    assert process_output_report([changed])['responses'][0]['process_to_observed_ns'] is None
assert all(item['process_to_observed_ns'] is None for item in process_output_report([row,row])['responses'])
"#;
    let output = std::process::Command::new("python3")
        .args(["-c", script])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run process-output report");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
