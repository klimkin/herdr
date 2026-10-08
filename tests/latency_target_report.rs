#[test]
fn target_report_distinguishes_delivery_floor_advance_from_retirement() {
    let script = r#"
import copy,sys
sys.path.insert(0,'scripts')
from latency_target_report import target_opportunities,target_deliveries
records=[
 {'stage':'opportunity.terminal_granted','pid':1,'id':7,'scope':31,'value':20,'runtime_instance':50,'ns':100},
 {'stage':'opportunity.input','pid':1,'id':9,'value':7,'service':90,'ns':100},
 {'stage':'opportunity.input_accepted','pid':1,'id':9,'value':1,'service':90,'ns':99},
 {'stage':'opportunity.target_pane','pid':1,'id':7,'value':41,'runtime_instance':50,'ns':100},
 {'stage':'opportunity.terminal_presented','pid':1,'id':7,'scope':4,'value':22,'runtime_instance':50,'presentation':101,'attempt':102,'serialization':103,'ns':125},
 {'stage':'opportunity.terminal_floor_advanced','pid':1,'id':7,'scope':4,'value':22,'runtime_instance':50,'presentation':101,'attempt':102,'serialization':103,'ns':126},
 {'stage':'surface.content','pid':1,'id':41,'scope':777,'value':22,'runtime_instance':50,'presentation':101,'attempt':102,'serialization':103,'ns':120},
 {'stage':'server.enqueue','pid':1,'id':777,'scope':60,'connection':30,'occurrence':6,'presentation':101,'attempt':102,'serialization':103,'ns':124},
 {'stage':'server.connection','pid':1,'id':30,'scope':4,'value':2,'ns':90},
 {'stage':'opportunity.expires_at','pid':1,'id':7,'value':200,'ns':100},
 {'stage':'server.frame_start','pid':1,'id':102,'presentation':101,'attempt':102,'ns':111},
 {'stage':'server.target_enqueued','pid':1,'id':41,'scope':4,'value':22,'runtime_instance':50,'presentation':101,'attempt':102,'serialization':103,'ns':125},
]
delivery=target_deliveries(records)[0]
assert delivery['status']=='ordinary_enqueued' and delivery['revision']==22
assert delivery['floor_advanced'] and not delivery['retired']
window=target_opportunities(records)[0]
assert window['status']=='pending' and window['presented_revision']==22

burst=copy.deepcopy(records)
burst.append({'stage':'opportunity.early_terminal_admitted','pid':1,'id':7,'presentation':101,'value':1,'ns':110})
assert target_deliveries(burst)[0]['status']=='early_enqueued'
invalid_first=copy.deepcopy(burst)
invalid_first[-1]['value']=2
assert target_deliveries(invalid_first)[0]['status']=='unassigned'
# A later early admission must not invalidate an earlier ordinary delivery.
later=[]
for record in records:
 if record['stage'] in ('opportunity.terminal_presented','surface.content','server.enqueue','server.frame_start','server.target_enqueued'):
  record=copy.deepcopy(record)
  record['ns']+=30
  for field in ('presentation','attempt','serialization'):
   if field in record:record[field]+=10
  if record['stage']=='server.frame_start':record['id']+=10
  if record['stage']=='server.enqueue':record['occurrence']+=1
  if record['stage'] in ('opportunity.terminal_presented','surface.content','server.target_enqueued'):record['value']=24
  later.append(record)
later.append({'stage':'opportunity.early_terminal_admitted','pid':1,'id':7,'presentation':111,'ns':140})
retirement=copy.deepcopy(next(r for r in later if r['stage']=='opportunity.terminal_presented'))
retirement['stage']='opportunity.terminal_enqueued'
later.append(retirement)

second=copy.deepcopy(later)
next(record for record in second if record['stage']=='opportunity.early_terminal_admitted')['value']=2
rows_burst=target_deliveries(burst+second)
assert [row['status'] for row in rows_burst]==['early_enqueued','early_enqueued'],rows_burst
assert [row['admission_ordinal'] for row in rows_burst]==[1,2]
assert target_opportunities(burst+second)[0]['status']=='early_enqueued'
malformed=copy.deepcopy(second)
next(record for record in malformed if record['stage']=='opportunity.early_terminal_admitted')['value']=1
assert target_deliveries(burst+malformed)[1]['status']=='unassigned'
rows=target_deliveries(records+later)
assert [row['status'] for row in rows]==['ordinary_enqueued','early_enqueued'],rows
assert rows[0]['floor_advanced'] and not rows[0]['retired']
assert rows[1]['retired'] and not rows[1]['floor_advanced']
for change in ('duplicate','missing_surface','wrong_runtime','wrong_queue','missing_success','missing_lifecycle','conflicting_lifecycle'):
 broken=copy.deepcopy(records)
 if change=='duplicate':broken.append(copy.deepcopy(records[4]))
 if change=='missing_surface':broken.remove(next(r for r in broken if r['stage']=='surface.content'))
 if change=='wrong_runtime':next(r for r in broken if r['stage']=='opportunity.terminal_presented')['runtime_instance']=51
 if change=='wrong_queue':next(r for r in broken if r['stage']=='server.enqueue')['occurrence']=0
 if change=='missing_success':broken.remove(next(r for r in broken if r['stage']=='server.target_enqueued'))
 if change=='missing_lifecycle':broken.remove(next(r for r in broken if r['stage']=='opportunity.terminal_floor_advanced'))
 if change=='conflicting_lifecycle':
  extra=copy.deepcopy(next(r for r in broken if r['stage']=='opportunity.terminal_floor_advanced'))
  extra['stage']='opportunity.terminal_enqueued'
  broken.append(extra)
 assert all(row['status']=='unassigned' for row in target_deliveries(broken)),change
 assert target_opportunities(broken)[0]['status']=='unassigned',change
records.append({'stage':'opportunity.expired','pid':1,'id':7,'ns':201})
assert target_opportunities(records)[0]['status']=='expired'
records[-1]={'stage':'opportunity.revoked','pid':1,'id':7,'ns':150}
assert target_opportunities(records)[0]['status']=='revoked'
"#;
    let output = std::process::Command::new("python3")
        .args(["-c", script])
        .output()
        .expect("public delivery report");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn target_report_requires_exact_accepted_runtime_revision_and_queue_occurrence() {
    let script = r#"
import copy,sys
sys.path.insert(0,'scripts')
from latency_target_report import target_opportunities
records=[
 {'stage':'opportunity.terminal_granted','pid':1,'id':7,'scope':31,'value':20,'runtime_instance':50,'ns':100},
 {'stage':'opportunity.input','pid':1,'id':9,'value':7,'service':90,'ns':100},
 {'stage':'opportunity.input_accepted','pid':1,'id':9,'value':1,'service':90,'ns':99},
 {'stage':'opportunity.target_pane','pid':1,'id':7,'value':41,'runtime_instance':50,'ns':100},
 {'stage':'opportunity.early_terminal_admitted','pid':1,'id':7,'presentation':101,'ns':110},
 {'stage':'opportunity.terminal_enqueued','pid':1,'id':7,'scope':4,'value':22,'runtime_instance':50,'presentation':101,'attempt':102,'serialization':103,'ns':125},
 {'stage':'surface.content','pid':1,'id':41,'scope':777,'value':22,'runtime_instance':50,'presentation':101,'attempt':102,'serialization':103,'ns':120},
 {'stage':'server.enqueue','pid':1,'id':777,'scope':60,'connection':30,'occurrence':6,'presentation':101,'attempt':102,'serialization':103,'ns':124},
 {'stage':'server.connection','pid':1,'id':30,'scope':4,'value':2,'ns':90},
 {'stage':'opportunity.expires_at','pid':1,'id':7,'value':200,'ns':100},
 {'stage':'server.frame_start','pid':1,'id':102,'presentation':101,'attempt':102,'ns':111},
 {'stage':'server.target_enqueued','pid':1,'id':41,'scope':4,'value':22,'runtime_instance':50,'presentation':101,'attempt':102,'serialization':103,'ns':125},
]
row=target_opportunities(records,strict=True)[0]
assert row['status']=='early_enqueued' and row['runtime_instance']==50
assert row['revision']==22 and row['pane']==41 and row['event']==9 and row['service']==90
assert row['attempt']==102 and row['serialization']==103 and row['connection']==30 and row['occurrence']==6
for change in ('runtime','odd','queue','duplicate','failed','missing_input'):
 broken=copy.deepcopy(records)
 if change=='runtime': next(r for r in broken if r['stage']=='opportunity.terminal_enqueued')['runtime_instance']=51
 if change=='odd': next(r for r in broken if r['stage']=='opportunity.terminal_enqueued')['value']=23
 if change=='queue': next(r for r in broken if r['stage']=='server.enqueue')['serialization']=104
 if change=='duplicate': broken.append(copy.deepcopy(next(r for r in broken if r['stage']=='server.enqueue')))
 if change=='failed': broken.append({'stage':'server.enqueue_full','pid':1,'serialization':103,'ns':124}) ; broken.remove(next(r for r in broken if r['stage']=='server.enqueue'))
 if change=='missing_input': broken.pop(1)
 assert target_opportunities(broken,strict=True)[0]['status']=='unassigned',change
# Completion is never fabricated from an admission or a later runtime revision.
assert target_opportunities(records[:4],strict=True)[0]['status']=='pending'
"#;
    let output = std::process::Command::new("python3")
        .args(["-c", script])
        .output()
        .expect("public report command");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn target_report_keeps_accepted_without_grant_empty_and_rejected_input() {
    let script = r#"
import sys
sys.path.insert(0,'scripts')
from latency_target_report import target_inputs
records=[
 {'stage':'opportunity.input_accepted','pid':1,'id':9,'scope':0,'value':1,'service':90,'ns':100},
 {'stage':'opportunity.input_no_baseline','pid':1,'id':9,'scope':0,'value':21,'service':90,'ns':101},
 {'stage':'opportunity.input_empty','pid':1,'id':10,'scope':0,'value':0,'service':91,'ns':110},
 {'stage':'opportunity.input_rejected','pid':1,'id':11,'scope':0,'value':0,'service':92,'ns':120},
]
rows=target_inputs(records)
assert len(rows)==3
assert rows[0]['status']=='accepted_without_grant' and rows[0]['event']==9 and rows[0]['service']==90
assert rows[0]['reason']=='opportunity.input_no_baseline'
assert rows[1]['status']=='empty' and rows[2]['status']=='rejected'
"#;
    let output = std::process::Command::new("python3")
        .args(["-c", script])
        .output()
        .expect("public input report");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn target_report_preserves_coalesced_inputs_without_guessing_echo_causality() {
    let script = r#"
import sys
sys.path.insert(0,'scripts')
from latency_target_report import target_opportunities
records=[
 {'stage':'opportunity.terminal_granted','pid':1,'id':7,'scope':31,'value':20,'runtime_instance':50,'ns':100},
 {'stage':'opportunity.input','pid':1,'id':9,'value':7,'service':90,'ns':100},
 {'stage':'opportunity.input_accepted','pid':1,'id':9,'value':1,'service':90,'ns':99},
 {'stage':'opportunity.input','pid':1,'id':10,'value':7,'service':91,'ns':105},
 {'stage':'opportunity.input_accepted','pid':1,'id':10,'value':1,'service':91,'ns':104},
 {'stage':'opportunity.target_pane','pid':1,'id':7,'value':41,'runtime_instance':50,'ns':100},
 {'stage':'opportunity.early_terminal_admitted','pid':1,'id':7,'presentation':101,'ns':110},
 {'stage':'opportunity.terminal_enqueued','pid':1,'id':7,'scope':4,'value':22,'runtime_instance':50,'presentation':101,'attempt':102,'serialization':103,'ns':125},
 {'stage':'surface.content','pid':1,'id':41,'scope':777,'value':22,'runtime_instance':50,'presentation':101,'attempt':102,'serialization':103,'ns':120},
 {'stage':'server.enqueue','pid':1,'id':777,'scope':60,'connection':30,'occurrence':6,'presentation':101,'attempt':102,'serialization':103,'ns':124},
 {'stage':'server.connection','pid':1,'id':30,'scope':4,'value':2,'ns':90},
 {'stage':'opportunity.expires_at','pid':1,'id':7,'value':200,'ns':100},
 {'stage':'server.frame_start','pid':1,'id':102,'presentation':101,'attempt':102,'ns':111},
 {'stage':'server.target_enqueued','pid':1,'id':41,'scope':4,'value':22,'runtime_instance':50,'presentation':101,'attempt':102,'serialization':103,'ns':125},
]
row=target_opportunities(records,strict=True)[0]
assert row['status']=='early_enqueued' and row['event']==9 and row['service']==90
assert row['inputs']==[{'event':9,'service':90},{'event':10,'service':91}]
assert row['echo_causality']=='unavailable'
records.append(dict(records[1]))
assert target_opportunities(records,strict=True)[0]['status']=='unassigned'
"#;
    let output = std::process::Command::new("python3")
        .args(["-c", script])
        .output()
        .expect("public coalesced report");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn target_report_rejects_malformed_identity_and_contradictory_admission() {
    let script = r#"
import copy,sys
sys.path.insert(0,'scripts')
from latency_target_report import target_opportunities
records=[
 {'stage':'opportunity.terminal_granted','pid':1,'id':7,'scope':31,'value':20,'runtime_instance':50,'ns':100},
 {'stage':'opportunity.input','pid':1,'id':9,'value':7,'service':90,'ns':100},
 {'stage':'opportunity.input_accepted','pid':1,'id':9,'value':1,'service':90,'ns':99},
 {'stage':'opportunity.target_pane','pid':1,'id':7,'value':41,'runtime_instance':50,'ns':100},
 {'stage':'opportunity.early_terminal_admitted','pid':1,'id':7,'presentation':101,'ns':110},
 {'stage':'opportunity.terminal_enqueued','pid':1,'id':7,'scope':4,'value':22,'runtime_instance':50,'presentation':101,'attempt':102,'serialization':103,'ns':125},
 {'stage':'surface.content','pid':1,'id':41,'scope':777,'value':22,'runtime_instance':50,'presentation':101,'attempt':102,'serialization':103,'ns':120},
 {'stage':'server.enqueue','pid':1,'id':777,'scope':60,'connection':30,'occurrence':6,'presentation':101,'attempt':102,'serialization':103,'ns':124},
 {'stage':'server.connection','pid':1,'id':30,'scope':4,'value':2,'ns':90},
 {'stage':'opportunity.expires_at','pid':1,'id':7,'value':200,'ns':100},
 {'stage':'server.frame_start','pid':1,'id':102,'presentation':101,'attempt':102,'ns':111},
 {'stage':'server.target_enqueued','pid':1,'id':41,'scope':4,'value':22,'runtime_instance':50,'presentation':101,'attempt':102,'serialization':103,'ns':125},
]
for change in ('boolean_id','huge_runtime','zero_pid','duplicate_grant','duplicate_admission','wrong_presentation','backward_ns'):
 broken=copy.deepcopy(records)
 if change=='boolean_id': broken[0]['id']=True
 if change=='huge_runtime': next(r for r in broken if r['stage']=='opportunity.terminal_enqueued')['runtime_instance']=1<<65
 if change=='zero_pid':
  for r in broken:r['pid']=0
 if change=='duplicate_grant':broken.append(dict(broken[0]))
 if change=='duplicate_admission':broken.append(dict(next(r for r in broken if r['stage']=='opportunity.early_terminal_admitted')))
 if change=='wrong_presentation':next(r for r in broken if r['stage']=='opportunity.early_terminal_admitted')['presentation']=104
 if change=='backward_ns':next(r for r in broken if r['stage']=='opportunity.terminal_enqueued')['ns']=99
 assert all(row['status']=='unassigned' for row in target_opportunities(broken,strict=True)),change
"#;
    let output = std::process::Command::new("python3")
        .args(["-c", script])
        .output()
        .expect("public malformed report");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn target_report_cli_marks_missing_process_finish_unavailable() {
    let script = r#"
import json,pathlib,subprocess,tempfile
with tempfile.TemporaryDirectory() as tmp:
 path=pathlib.Path(tmp)/'1.jsonl'
 path.write_text(json.dumps({'stage':'opportunity.terminal_granted','pid':1,'id':7,'scope':31,'value':20,'runtime_instance':50,'ns':100})+'\n')
 result=subprocess.run(['python3','scripts/latency_target_report.py',str(path)],capture_output=True,text=True,check=True)
 report=json.loads(result.stdout)
 assert report['integrity']['all_expected_complete'] is False
 assert report['opportunities'][0]['status']=='unassigned'
 assert 'missing finish marker' in report['integrity']['processes'][0]['issues']
"#;
    let output = std::process::Command::new("python3")
        .args(["-c", script])
        .output()
        .expect("public integrity report");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn target_report_requires_live_expiry_successful_attempt_and_recipient_lease() {
    let script = r#"
import copy,sys
sys.path.insert(0,'scripts')
from latency_target_report import target_opportunities
records=[
 {'stage':'opportunity.terminal_granted','pid':1,'id':7,'scope':31,'value':20,'runtime_instance':50,'ns':100},
 {'stage':'opportunity.input','pid':1,'id':9,'value':7,'service':90,'ns':100},
 {'stage':'opportunity.input_accepted','pid':1,'id':9,'value':1,'service':90,'ns':99},
 {'stage':'opportunity.target_pane','pid':1,'id':7,'value':41,'runtime_instance':50,'ns':100},
 {'stage':'opportunity.early_terminal_admitted','pid':1,'id':7,'presentation':101,'ns':110},
 {'stage':'opportunity.terminal_enqueued','pid':1,'id':7,'scope':4,'value':22,'runtime_instance':50,'presentation':101,'attempt':102,'serialization':103,'ns':125},
 {'stage':'surface.content','pid':1,'id':41,'scope':777,'value':22,'runtime_instance':50,'presentation':101,'attempt':102,'serialization':103,'ns':120},
 {'stage':'server.enqueue','pid':1,'id':777,'scope':60,'connection':30,'occurrence':6,'presentation':101,'attempt':102,'serialization':103,'ns':124},
 {'stage':'server.connection','pid':1,'id':30,'scope':4,'value':2,'ns':90},
 {'stage':'opportunity.expires_at','pid':1,'id':7,'value':200,'ns':100},
 {'stage':'server.frame_start','pid':1,'id':102,'presentation':101,'attempt':102,'ns':111},
 {'stage':'server.target_enqueued','pid':1,'id':41,'scope':4,'value':22,'runtime_instance':50,'presentation':101,'attempt':102,'serialization':103,'ns':125},
]
assert target_opportunities(records,strict=True)[0]['status']=='early_enqueued'
for change in ('expired','wrong_recipient','missing_begin','failed_attempt','missing_success','missing_expiry'):
 broken=copy.deepcopy(records)
 if change=='expired':next(r for r in broken if r['stage']=='opportunity.expires_at')['value']=124
 if change=='wrong_recipient':next(r for r in broken if r['stage']=='server.connection')['scope']=5
 if change=='missing_begin':broken.remove(next(r for r in broken if r['stage']=='server.frame_start'))
 if change=='failed_attempt':broken.append({'stage':'server.frame_fallback.geometry','pid':1,'id':102,'attempt':102,'presentation':101,'ns':121})
 if change=='missing_success':broken.remove(next(r for r in broken if r['stage']=='server.target_enqueued'))
 if change=='missing_expiry':broken.remove(next(r for r in broken if r['stage']=='opportunity.expires_at'))
 assert target_opportunities(broken,strict=True)[0]['status']=='unassigned',change
"#;
    let output = std::process::Command::new("python3")
        .args(["-c", script])
        .output()
        .expect("public strict report");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn target_report_keeps_failed_extra_before_ordinary_receipt() {
    let script = r#"
import sys
sys.path.insert(0,'scripts')
from latency_target_report import target_opportunities
records=[
 {'stage':'opportunity.terminal_granted','pid':1,'id':7,'scope':31,'value':20,'runtime_instance':50,'ns':100},
 {'stage':'opportunity.input_accepted','pid':1,'id':9,'value':1,'service':90,'ns':99},
 {'stage':'opportunity.input','pid':1,'id':9,'value':7,'service':90,'ns':100},
 {'stage':'opportunity.target_pane','pid':1,'id':7,'value':41,'runtime_instance':50,'ns':100},
 {'stage':'opportunity.early_terminal_admitted','pid':1,'id':7,'presentation':90,'ns':105},
 {'stage':'server.frame_start','pid':1,'id':91,'attempt':91,'presentation':90,'ns':106},
 {'stage':'server.frame_fallback.geometry','pid':1,'id':91,'attempt':91,'presentation':90,'ns':107},
 {'stage':'opportunity.terminal_enqueued','pid':1,'id':7,'scope':4,'value':22,'runtime_instance':50,'presentation':101,'attempt':102,'serialization':103,'ns':125},
 {'stage':'surface.content','pid':1,'id':41,'scope':777,'value':22,'runtime_instance':50,'presentation':101,'attempt':102,'serialization':103,'ns':120},
 {'stage':'server.enqueue','pid':1,'id':777,'scope':60,'connection':30,'occurrence':6,'presentation':101,'attempt':102,'serialization':103,'ns':124},
 {'stage':'server.connection','pid':1,'id':30,'scope':4,'value':2,'ns':90},
 {'stage':'opportunity.expires_at','pid':1,'id':7,'value':200,'ns':100},
 {'stage':'server.frame_start','pid':1,'id':102,'presentation':101,'attempt':102,'ns':111},
 {'stage':'server.target_enqueued','pid':1,'id':41,'scope':4,'value':22,'runtime_instance':50,'presentation':101,'attempt':102,'serialization':103,'ns':125},
]
row=target_opportunities(records)[0]
assert row['status']=='ordinary_enqueued'
assert row['prior_admission']=={'presentation':90,'ns':105,'outcome':'fallback'}
"#;
    let output = std::process::Command::new("python3")
        .args(["-c", script])
        .output()
        .expect("public later ordinary report");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn target_report_rejects_conflicting_admission_and_queue_histories() {
    let script = r#"
import copy,sys
sys.path.insert(0,'scripts')
from latency_target_report import target_opportunities,target_inputs
records=[
 {'stage':'opportunity.terminal_granted','pid':1,'id':7,'scope':31,'value':20,'runtime_instance':50,'ns':100},
 {'stage':'opportunity.input_accepted','pid':1,'id':9,'value':1,'service':90,'ns':99},
 {'stage':'opportunity.input','pid':1,'id':9,'value':7,'service':90,'ns':100},
 {'stage':'opportunity.target_pane','pid':1,'id':7,'value':41,'runtime_instance':50,'ns':100},
 {'stage':'opportunity.early_terminal_admitted','pid':1,'id':7,'presentation':101,'ns':110},
 {'stage':'opportunity.terminal_enqueued','pid':1,'id':7,'scope':4,'value':22,'runtime_instance':50,'presentation':101,'attempt':102,'serialization':103,'ns':125},
 {'stage':'surface.content','pid':1,'id':41,'scope':777,'value':22,'runtime_instance':50,'presentation':101,'attempt':102,'serialization':103,'ns':120},
 {'stage':'server.enqueue','pid':1,'id':777,'scope':60,'connection':30,'occurrence':6,'presentation':101,'attempt':102,'serialization':103,'ns':124},
 {'stage':'server.connection','pid':1,'id':30,'scope':4,'value':2,'ns':90},
 {'stage':'opportunity.expires_at','pid':1,'id':7,'value':200,'ns':100},
 {'stage':'server.frame_start','pid':1,'id':102,'presentation':101,'attempt':102,'ns':111},
 {'stage':'server.target_enqueued','pid':1,'id':41,'scope':4,'value':22,'runtime_instance':50,'presentation':101,'attempt':102,'serialization':103,'ns':125},
]
assert target_opportunities(records)[0]['status']=='early_enqueued'
for change in ('missing_accept','future_input','future_admit','future_success','future_connection','queue_failed','duplicate_occurrence','rejected_link','dangling_link'):
 broken=copy.deepcopy(records)
 if change=='missing_accept':broken.pop(1)
 if change=='future_input':broken[2]['ns']=130
 if change=='future_admit':broken[4]['ns']=130
 if change=='future_success':broken[11]['ns']=130
 if change=='future_connection':broken[8]['ns']=130
 if change=='queue_failed':broken.append(dict(broken[7],stage='server.enqueue_full'))
 if change=='duplicate_occurrence':broken.append(dict(broken[7],serialization=104))
 if change=='rejected_link':broken.append(dict(broken[1],stage='opportunity.input_rejected'))
 if change=='dangling_link':broken[2]['value']=8
 assert target_opportunities(broken)[0]['status']=='unassigned',change
 if change in ('rejected_link','dangling_link'):
  assert all(row['status']=='unassigned' for row in target_inputs(broken)),change
"#;
    let output = std::process::Command::new("python3")
        .args(["-c", script])
        .output()
        .expect("public conflicting report");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn target_report_validates_all_input_outcomes_even_without_grants() {
    let script = r#"
import copy,sys
sys.path.insert(0,'scripts')
from latency_target_report import target_inputs
base={'stage':'opportunity.input_empty','pid':1,'id':9,'value':0,'service':90,'ns':100}
assert target_inputs([base])[0]['status']=='empty'
for change in ('zero_id','bad_service','duplicate','contradiction','dangling','duplicate_links','before_admission'):
 records=[copy.deepcopy(base)]
 if change=='zero_id':records[0]['id']=0
 if change=='bad_service':records[0]['service']=True
 if change=='duplicate':records.append(dict(base))
 if change=='contradiction':records.append(dict(base,stage='opportunity.input_accepted',value=1))
 if change=='dangling':records.append(dict(base,stage='opportunity.input',value=7))
 if change=='duplicate_links':records += [dict(base,stage='opportunity.input',value=7)]*2
 if change=='before_admission':records.append(dict(base,stage='opportunity.input',value=7,ns=99))
 assert all(row['status']=='unassigned' for row in target_inputs(records)),change
"#;
    let output = std::process::Command::new("python3")
        .args(["-c", script])
        .output()
        .expect("public input integrity report");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn target_report_keeps_comparator_undo_unpresented() {
    let script = r#"
import sys
sys.path.insert(0,'scripts')
from latency_target_report import target_opportunities
records=[
 {'stage':'opportunity.terminal_granted','pid':1,'id':7,'scope':31,'value':20,'runtime_instance':50,'ns':100},
 {'stage':'opportunity.input_accepted','pid':1,'id':9,'value':1,'service':90,'ns':99},
 {'stage':'opportunity.input','pid':1,'id':9,'value':7,'service':90,'ns':100},
 {'stage':'opportunity.target_pane','pid':1,'id':7,'value':41,'runtime_instance':50,'ns':100},
 {'stage':'opportunity.early_terminal_admitted','pid':1,'id':7,'presentation':101,'ns':110},
 {'stage':'server.target_material_absent','pid':1,'id':41,'scope':4,'value':22,'runtime_instance':50,'presentation':101,'attempt':102,'serialization':0,'ns':120},
 {'stage':'surface.content','pid':1,'id':41,'scope':777,'value':22,'runtime_instance':50,'presentation':101,'attempt':102,'serialization':103,'ns':120},
 {'stage':'server.enqueue','pid':1,'id':777,'scope':60,'connection':30,'occurrence':6,'presentation':101,'attempt':102,'serialization':103,'ns':124},
]
row=target_opportunities(records)[0]
assert row['status']=='pending'
assert row['attempt_outcomes']==['material_absent']
"#;
    let output = std::process::Command::new("python3")
        .args(["-c", script])
        .output()
        .expect("public comparator undo report");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn target_input_membership_rejects_invalid_grant_identity() {
    let script = r#"
import copy,sys
sys.path.insert(0,'scripts')
from latency_target_report import target_inputs
records=[
 {'stage':'opportunity.input_accepted','pid':1,'id':9,'value':1,'service':90,'ns':99},
 {'stage':'opportunity.input','pid':1,'id':9,'value':7,'service':90,'ns':100},
 {'stage':'opportunity.terminal_granted','pid':1,'id':7,'scope':31,'value':20,'runtime_instance':50,'ns':100},
]
assert target_inputs(records)[0]['status']=='granted'
for field,value in [('scope',0),('runtime_instance',0),('value',21),('id',True)]:
 broken=copy.deepcopy(records);broken[2][field]=value
 assert target_inputs(broken)[0]['status']=='unassigned',field
"#;
    let output = std::process::Command::new("python3")
        .args(["-c", script])
        .output()
        .expect("public grant membership report");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
