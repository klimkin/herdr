#[test]
fn committed_action_report_keeps_admission_distinct_from_delivery() {
    let script = r#"
import sys
sys.path.insert(0, 'scripts')
from latency_action_report import action_opportunities
records=[
 {'stage':'server.action_committed','pid':1,'id':31,'value':41,'ns':90},
 {'stage':'opportunity.action_granted','pid':1,'id':7,'scope':31,'value':41,'ns':100},
 {'stage':'opportunity.action_target','pid':1,'id':7,'scope':0,'value':51,'ns':100},
 {'stage':'opportunity.admitted','pid':1,'id':7,'scope':0,'value':0,'ns':200,'presentation':10},
 {'stage':'opportunity.action_enqueued','pid':1,'id':7,'scope':61,'value':3,'ns':300,'presentation':10,'attempt':12},
 {'stage':'server.frame_start','pid':1,'id':12,'ns':250,'presentation':10,'attempt':12},
]

records.extend([{'stage':'opportunity.accepted_at','pid':1,'id':7,'scope':0,'value':100,'ns':100}, {'stage':'opportunity.expires_at','pid':1,'id':7,'scope':0,'value':16_000_100,'ns':100}])
rows=action_opportunities(records)
assert len(rows)==1,rows
assert rows[0]['opportunity']==7 and rows[0]['request']==31
assert rows[0]['label']==41 and rows[0]['target']==51
assert rows[0]['admitted_ns']==200
assert rows[0]['enqueue_ns']==300 and rows[0]['attempt']==12
assert rows[0]['status']=='early_enqueued'
assert rows[0]['output_completed'] is False
"#;
    let status = std::process::Command::new("python3")
        .args(["-c", script])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .expect("public report executable");
    assert!(status.success());
}

#[test]
fn committed_action_report_rejects_mismatched_or_expired_receipts() {
    let script = r#"
import copy,sys
sys.path.insert(0, 'scripts')
from latency_action_report import action_opportunities
base=[
 {'stage':'server.action_committed','pid':1,'id':31,'value':41,'ns':90},
 {'stage':'opportunity.action_granted','pid':1,'id':7,'scope':31,'value':41,'ns':100},
 {'stage':'opportunity.action_target','pid':1,'id':7,'scope':0,'value':51,'ns':100},
 {'stage':'opportunity.admitted','pid':1,'id':7,'scope':0,'value':0,'ns':200,'presentation':10},
 {'stage':'server.frame_start','pid':1,'id':12,'ns':250,'presentation':10,'attempt':12},
 {'stage':'opportunity.action_enqueued','pid':1,'id':7,'scope':61,'value':3,'ns':300,'presentation':10,'attempt':12},
]

times=[{'stage':'opportunity.accepted_at','pid':1,'id':7,'scope':0,'value':100,'ns':100}, {'stage':'opportunity.expires_at','pid':1,'id':7,'scope':0,'value':16_000_100,'ns':100}]
assert action_opportunities(base+times)[0]['status']=='early_enqueued'
for change in ('presentation','attempt','ordering','expiry','duplicate_grant','superseded'):
 bad=copy.deepcopy(base)
 if change=='presentation':bad[-1]['presentation']=20
 elif change=='attempt':bad[-1]['attempt']=99
 elif change=='ordering':bad[-1]['ns']=150
 elif change=='expiry':bad[-1]['ns']=16_000_101
 elif change=='duplicate_grant':bad.append(copy.copy(bad[1]))
 else:bad.append({'stage':'opportunity.superseded','pid':1,'id':7,'ns':175})
 assert action_opportunities(bad+times)[0]['status']!='early_enqueued',(change,action_opportunities(bad+times))
ordinary=[r for r in base if r['stage']!='opportunity.admitted']
assert action_opportunities(ordinary+times)[0]['status']=='ordinary_enqueued'
assert action_opportunities(ordinary+times)[0]['output_completed'] is False
"#;
    let status = std::process::Command::new("python3")
        .args(["-c", script])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .expect("public report executable");
    assert!(status.success());
}

#[test]
fn committed_action_report_requires_nonzero_unique_commit_chain() {
    let script = r#"
import copy,sys
sys.path.insert(0, 'scripts')
from latency_action_report import action_opportunities
base=[
 {'stage':'server.action_committed','pid':1,'id':31,'value':41,'ns':90},
 {'stage':'opportunity.action_granted','pid':1,'id':7,'scope':31,'value':41,'ns':100},
 {'stage':'opportunity.action_target','pid':1,'id':7,'scope':0,'value':51,'ns':100},
 {'stage':'opportunity.admitted','pid':1,'id':7,'scope':0,'value':0,'ns':200,'presentation':10},
 {'stage':'server.frame_start','pid':1,'id':12,'ns':250,'presentation':10,'attempt':12},
 {'stage':'opportunity.action_enqueued','pid':1,'id':7,'scope':61,'value':3,'ns':300,'presentation':10,'attempt':12},
]

times=[{'stage':'opportunity.accepted_at','pid':1,'id':7,'scope':0,'value':100,'ns':100}, {'stage':'opportunity.expires_at','pid':1,'id':7,'scope':0,'value':16_000_100,'ns':100}]
assert action_opportunities(base+times)[0]['status']=='early_enqueued'
for change in ('missing_commit','duplicate_commit','late_commit','wrong_label','wrong_pid','zero_request','zero_target','zero_presentation','zero_attempt'):
 bad=copy.deepcopy(base)
 if change=='missing_commit':bad.pop(0)
 elif change=='duplicate_commit':bad.append(copy.copy(bad[0]))
 elif change=='late_commit':bad[0]['ns']=110
 elif change=='wrong_label':bad[0]['value']=42
 elif change=='wrong_pid':bad[0]['pid']=2
 elif change=='zero_request':bad[1]['scope']=0
 elif change=='zero_target':bad[2]['value']=0
 elif change=='zero_presentation':bad[3]['presentation']=bad[4]['presentation']=bad[5]['presentation']=0
 else:bad[4]['attempt']=bad[5]['attempt']=0
 assert action_opportunities(bad+times)[0]['status']!='early_enqueued',(change,action_opportunities(bad+times))
"#;
    let status = std::process::Command::new("python3")
        .args(["-c", script])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .expect("public report executable");
    assert!(status.success());
}

#[cfg(all(target_os = "linux", feature = "latency-prof"))]
#[test]
#[ignore = "build optimized owned-PTY helpers first; run just latency-action-behavior"]
fn accepted_actions_present_authoritative_labels_without_redundant_frames() {
    let target = std::env::var_os("CARGO_TARGET_DIR").unwrap_or_else(|| "target".into());
    let release = std::path::PathBuf::from(target).join("release");
    let bench = std::env::var_os("HERDR_ACTION_TEST_BENCH")
        .unwrap_or_else(|| release.join("examples/latency_bench").into());
    let probe = std::env::var_os("HERDR_ACTION_TEST_PROBE")
        .unwrap_or_else(|| release.join("examples/latency_probe").into());
    let binary = std::env::var_os("HERDR_ACTION_TEST_BINARY")
        .unwrap_or_else(|| release.join("herdr").into());
    let base = std::env::temp_dir().join(format!("herdr-action-contract-{}", std::process::id()));
    std::fs::create_dir_all(&base).expect("owned artifacts");
    let output = std::process::Command::new(bench)
        .env("HERDR_LATENCY_PRESENTATION", "action-full")
        .env("HERDR_LATENCY_TRACE_DIR", "enabled")
        .args(["--binary"])
        .arg(binary)
        .args(["--probe"])
        .arg(probe)
        .args(["--output"])
        .arg(&base)
        .args([
            "--path",
            "action",
            "--load",
            "quiet",
            "--samples",
            "8",
            "--warmups",
            "0",
            "--interval-ms",
            "40",
            "--action-entry",
            "json",
            "--action-phase",
            "output",
        ])
        .output()
        .expect("owned server/TUI benchmark");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let run = std::fs::read_dir(&base)
        .expect("run artifacts")
        .next()
        .expect("one run")
        .expect("run path")
        .path();
    let samples: serde_json::Value =
        serde_json::from_slice(&std::fs::read(run.join("samples.json")).expect("public samples"))
            .expect("sample JSON");
    assert_eq!(samples["summary"][0]["completed"], 8);
    assert_eq!(samples["summary"][0]["missing"], 0);
    let mut records = Vec::<serde_json::Value>::new();
    for file in std::fs::read_dir(run.join("traces")).expect("public traces") {
        let raw = std::fs::read_to_string(file.expect("trace file").path()).expect("trace text");
        records.extend(
            raw.lines()
                .map(|line| serde_json::from_str(line).expect("trace JSON")),
        );
    }
    assert!(records.iter().any(|r| r["stage"] == "opportunity.admitted"));
    let admitted: std::collections::HashSet<_> = records
        .iter()
        .filter(|r| r["stage"] == "opportunity.admitted")
        .filter_map(|r| r["presentation"].as_u64())
        .collect();
    for receipt in records.iter().filter(|r| {
        r["stage"] == "opportunity.action_enqueued"
            && r["presentation"]
                .as_u64()
                .is_some_and(|id| admitted.contains(&id))
    }) {
        let enqueue = receipt["ns"].as_u64().expect("enqueue time");
        assert!(
            !records.iter().any(|r| r["stage"] == "server.frame_start"
                && r["ns"]
                    .as_u64()
                    .is_some_and(|ns| enqueue < ns && ns < enqueue + 20_000_000)),
            "delivered action leaves redundant ordinary construction"
        );
    }
    std::fs::remove_dir_all(base).expect("owned artifact cleanup");
}

#[test]
fn committed_action_report_rejects_malformed_and_pregrant_frames() {
    let script = r#"
import copy,sys
sys.path.insert(0, 'scripts')
from latency_action_report import action_opportunities
base=[
 {'stage':'server.action_committed','pid':1,'id':31,'value':41,'ns':90},
 {'stage':'opportunity.action_granted','pid':1,'id':7,'scope':31,'value':41,'ns':100},
 {'stage':'opportunity.action_target','pid':1,'id':7,'scope':0,'value':51,'ns':100},
 {'stage':'opportunity.admitted','pid':1,'id':7,'scope':0,'value':0,'ns':200,'presentation':10},
 {'stage':'server.frame_start','pid':1,'id':12,'ns':250,'presentation':10,'attempt':12},
 {'stage':'opportunity.action_enqueued','pid':1,'id':7,'scope':61,'value':3,'ns':300,'presentation':10,'attempt':12},
]

times=[{'stage':'opportunity.accepted_at','pid':1,'id':7,'scope':0,'value':100,'ns':100}, {'stage':'opportunity.expires_at','pid':1,'id':7,'scope':0,'value':16_000_100,'ns':100}]
for value in (True, '12', -1, 1<<64):
 bad=copy.deepcopy(base);bad[-1]['attempt']=value
 assert action_opportunities(bad+times)[0]['status']=='unassigned'
for value in (True, '300', -1, 1<<64):
 bad=copy.deepcopy(base);bad[-1]['ns']=value
 assert action_opportunities(bad+times)[0]['status']=='unassigned'
ordinary=[r for r in copy.deepcopy(base) if r['stage']!='opportunity.admitted']
ordinary[-2]['ns']=99
assert action_opportunities(ordinary+times)[0]['status']=='unassigned'
"#;
    let status = std::process::Command::new("python3")
        .args(["-c", script])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .expect("public report executable");
    assert!(status.success());
}

#[test]
fn committed_action_report_retains_invalid_records_as_unassigned() {
    let script = r#"
import copy,sys
sys.path.insert(0, 'scripts')
from latency_action_report import action_opportunities
base=[
 {'stage':'server.action_committed','pid':1,'id':31,'value':41,'ns':90},
 {'stage':'opportunity.action_granted','pid':1,'id':7,'scope':31,'value':41,'ns':100},
 {'stage':'opportunity.action_target','pid':1,'id':7,'scope':0,'value':51,'ns':100},
 {'stage':'opportunity.admitted','pid':1,'id':7,'scope':0,'value':0,'ns':200,'presentation':10},
 {'stage':'server.frame_start','pid':1,'id':12,'ns':250,'presentation':10,'attempt':12},
 {'stage':'opportunity.action_enqueued','pid':1,'id':7,'scope':61,'value':3,'ns':300,'presentation':10,'attempt':12},
]

times=[{'stage':'opportunity.accepted_at','pid':1,'id':7,'scope':0,'value':100,'ns':100}, {'stage':'opportunity.expires_at','pid':1,'id':7,'scope':0,'value':16_000_100,'ns':100}]
for change in ('recipient','projection','frame_id','grant_ns','scope_list','id_list','stage_list','duplicate_enqueue'):
 bad=copy.deepcopy(base)
 if change=='recipient':bad[-1]['scope']=0
 elif change=='projection':bad[-1]['value']=0
 elif change=='frame_id':bad[-2]['id']=13
 elif change=='grant_ns':bad[1].pop('ns')
 elif change=='scope_list':bad[1]['scope']=[]
 elif change=='id_list':bad[1]['id']=[]
 elif change=='stage_list':bad[1]['stage']=[]
 else:bad.append(copy.copy(bad[-1]))
 rows=action_opportunities(bad+times)
 assert rows and any(r['status']=='unassigned' for r in rows),(change,rows)
"#;
    let status = std::process::Command::new("python3")
        .args(["-c", script])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .expect("public report executable");
    assert!(status.success());
}

#[test]
fn committed_action_report_distinguishes_budget_wait_and_revocation() {
    let script = r#"
import sys
sys.path.insert(0, 'scripts')
from latency_action_report import action_opportunities
base=[
 {'stage':'server.action_committed','pid':1,'id':31,'value':41,'ns':90},
 {'stage':'opportunity.action_granted','pid':1,'id':7,'scope':31,'value':41,'ns':100},
 {'stage':'opportunity.action_target','pid':1,'id':7,'scope':0,'value':51,'ns':100},
]

times=[{'stage':'opportunity.accepted_at','pid':1,'id':7,'scope':0,'value':100,'ns':100}, {'stage':'opportunity.expires_at','pid':1,'id':7,'scope':0,'value':16_000_100,'ns':100}]
for stage,status in [('opportunity.token_denied','waiting_for_token'),('opportunity.revoked','revoked'),('opportunity.expired','expired')]:
 records=base+times+[{'stage':stage,'pid':1,'id':7,'scope':0,'value':0,'ns':200}]
 assert action_opportunities(records)[0]['status']==status,action_opportunities(records)
"#;
    let status = std::process::Command::new("python3")
        .args(["-c", script])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .expect("public report executable");
    assert!(status.success());
}

#[test]
fn committed_action_report_uses_fixed_accepted_expiry() {
    let script = r#"
import copy,sys
sys.path.insert(0, 'scripts')
from latency_action_report import action_opportunities
base=[
 {'stage':'server.action_committed','pid':1,'id':31,'value':41,'ns':90},
 {'stage':'opportunity.action_granted','pid':1,'id':7,'scope':31,'value':41,'ns':110},
 {'stage':'opportunity.action_target','pid':1,'id':7,'scope':0,'value':51,'ns':110},
 {'stage':'opportunity.accepted_at','pid':1,'id':7,'scope':0,'value':100,'ns':115},
 {'stage':'opportunity.expires_at','pid':1,'id':7,'scope':0,'value':16_000_100,'ns':115},
 {'stage':'opportunity.admitted','pid':1,'id':7,'scope':0,'value':0,'ns':200,'presentation':10},
 {'stage':'server.frame_start','pid':1,'id':12,'ns':250,'presentation':10,'attempt':12},
 {'stage':'opportunity.action_enqueued','pid':1,'id':7,'scope':61,'value':3,'ns':300,'presentation':10,'attempt':12},
]
row=action_opportunities(base)[0]
assert row['accepted_ns']==100 and row['expires_ns']==16_000_100,row
assert row['status']=='early_enqueued'
for change in ('late','missing','renewed','duplicate_conflict'):
 bad=copy.deepcopy(base)
 if change=='late':bad[-1]['ns']=16_000_105
 elif change=='missing':bad.pop(4)
 elif change=='renewed':bad[4]['value']+=1
 else:bad.append(dict(bad[4],value=16_000_101))
 assert action_opportunities(bad)[0]['status']=='unassigned',(change,action_opportunities(bad))
"#;
    let status = std::process::Command::new("python3")
        .args(["-c", script])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .expect("public report executable");
    assert!(status.success());
}

#[test]
fn committed_action_report_rejects_attempts_before_recorded_grant() {
    let script = r#"
import copy,sys
sys.path.insert(0, 'scripts')
from latency_action_report import action_opportunities
base=[
 {'stage':'server.action_committed','pid':1,'id':31,'value':41,'ns':90},
 {'stage':'opportunity.action_granted','pid':1,'id':7,'scope':31,'value':41,'ns':110},
 {'stage':'opportunity.action_target','pid':1,'id':7,'scope':0,'value':51,'ns':110},
 {'stage':'opportunity.accepted_at','pid':1,'id':7,'scope':0,'value':100,'ns':115},
 {'stage':'opportunity.expires_at','pid':1,'id':7,'scope':0,'value':16_000_100,'ns':115},
 {'stage':'opportunity.admitted','pid':1,'id':7,'scope':0,'value':0,'ns':200,'presentation':10},
 {'stage':'server.frame_start','pid':1,'id':12,'ns':250,'presentation':10,'attempt':12},
 {'stage':'opportunity.action_enqueued','pid':1,'id':7,'scope':61,'value':3,'ns':300,'presentation':10,'attempt':12},
]
for index in (5,6,7):
 bad=copy.deepcopy(base);bad[index]['ns']=105
 assert action_opportunities(bad)[0]['status']=='unassigned',(index,action_opportunities(bad))
ordinary=[r for r in copy.deepcopy(base) if r['stage']!='opportunity.admitted']
ordinary[-2]['ns']=105
assert action_opportunities(ordinary)[0]['status']=='unassigned'
"#;
    let status = std::process::Command::new("python3")
        .args(["-c", script])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .expect("public report executable");
    assert!(status.success());
}
