#[test]
fn latency_listener_cpu_report_rejects_missing_roles_and_regressed_samples() {
    let script = r#"
import sys
sys.path.insert(0, 'scripts')
from latency_listener_cpu_report import cpu_report

def row(pid,ticks,left,right):
    return {'pid':pid,'start_ticks':pid+100,'ticks':ticks,'sample_start_ns':left,
            'sample_end_ns':right,'threads':[], 'command':['herdr'],
            'voluntary_switches':0,'involuntary_switches':0,'stdin_target':None}
points=[{'ns':0,'sample_end_ns':10,'collector_cpu_ns':0,
         'processes':[row(1,0,1,2),row(2,0,3,4)]},
        {'ns':1_000_000_000,'sample_end_ns':1_000_000_010,'collector_cpu_ns':0,
         'processes':[row(1,100,1_000_000_001,1_000_000_002),
                      row(2,0,1_000_000_003,1_000_000_004)]}]
run={'server_pid':1,'client_pids':[2]}
r=cpu_report(points,0,1_000_000_020,run,3,tick_hz=100,minimum_seconds=1)
assert r['runtime_process_coverage_complete'], r
assert r['cpu_percent']['server'] == 100.0, r
assert r['total_herdr_cpu_percent'] == 100.0, r
broken=[{**point,'processes':list(point['processes'])}for point in points]
broken[-1]['processes']=broken[-1]['processes'][:1]
r=cpu_report(broken,0,1_000_000_020,run,3,tick_hz=100,minimum_seconds=1)
assert not r['runtime_process_coverage_complete'] and r['total_herdr_cpu_percent'] is None, r
broken=[{**point,'processes':[dict(row)for row in point['processes']]}for point in points]
broken[-1]['processes'][0]['ticks']=-1
r=cpu_report(broken,0,1_000_000_020,run,3,tick_hz=100,minimum_seconds=1)
assert r['validation_errors'] and r['total_herdr_cpu_percent'] is None, r
# A declared183-second window cannot hide180seconds of missing CPU scans.
sparse=[points[0],{**points[1],'ns':183_000_000_000,'sample_end_ns':183_000_000_010,
                 'processes':[row(1,100,183_000_000_001,183_000_000_002),row(2,0,183_000_000_003,183_000_000_004)]}]
r=cpu_report(sparse,0,183_000_000_020,run,3,tick_hz=100,minimum_seconds=180)
assert not r['runtime_process_coverage_complete'] and r['total_herdr_cpu_percent'] is None, r
for bad_points in (list(reversed(points)),[points[0],points[0],points[1]],
                   [{**points[0],'processes':points[0]['processes']+[points[0]['processes'][0]]},points[1]]):
    r=cpu_report(bad_points,0,1_000_000_020,run,3,tick_hz=100,minimum_seconds=1)
    assert not r['runtime_process_coverage_complete'], r
for ticks in (True,0,-1,float('nan')):
    r=cpu_report(points,0,1_000_000_020,run,3,tick_hz=ticks,minimum_seconds=1)
    assert not r['runtime_process_coverage_complete'], r
bad=[{**point,'processes':[dict(row)for row in point['processes']]}for point in points]
bad[0]['processes'][0]['start_ticks']=0
assert not cpu_report(bad,0,1_000_000_020,run,3,tick_hz=100,minimum_seconds=1)['runtime_process_coverage_complete']
assert not cpu_report(points,0,1_000_000_020,run,3,tick_hz=100,minimum_seconds=float('nan'))['runtime_process_coverage_complete']
"#;
    let output = std::process::Command::new("python3")
        .args(["-c", script])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run public CPU report");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
