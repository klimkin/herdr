"""Report exact server opportunities; enqueue remains separate from output."""
from collections import defaultdict

EXPIRY_NS = 16_000_000


def unsigned(value, nonzero=False):
    return type(value) is int and int(nonzero) <= value < 1 << 64


def record_valid(record):
    return all(unsigned(record.get(name)) for name in ('pid', 'id', 'ns')) and all(
        unsigned(record.get(name, 0)) for name in ('scope', 'value', 'presentation', 'attempt'))


def action_opportunities(records):
    opportunities = defaultdict(list)
    attempts = defaultdict(list)
    commits = defaultdict(list)
    malformed = []
    for record in records:
        if (not isinstance(record, dict) or not isinstance(record.get('stage'), str)
                or not record_valid(record)):
            malformed.append({'opportunity': record.get('id') if isinstance(record, dict) else None,
                              'status': 'unassigned', 'reason': 'malformed diagnostic record',
                              'output_completed': False})
            continue
        stage = record['stage']
        if stage.startswith('opportunity.'):
            opportunities[(record.get('pid'), record.get('id'))].append(record)
        elif stage == 'server.action_committed':
            commits[(record.get('pid'), record.get('id'), record.get('value'))].append(record)
        elif stage == 'server.frame_start':
            attempts[(record.get('pid'), record.get('attempt'))].append(record)
    rows = malformed
    for (pid, identity), same in opportunities.items():
        grants = [r for r in same if r['stage'] == 'opportunity.action_granted']
        if not grants:
            continue
        grant = grants[0]
        targets = [r for r in same if r['stage'] == 'opportunity.action_target']
        admissions = [r for r in same if r['stage'] == 'opportunity.admitted']
        enqueues = [r for r in same if r['stage'] == 'opportunity.action_enqueued']
        admission = admissions[0] if len(admissions) == 1 else None
        enqueue = enqueues[0] if len(enqueues) == 1 else None
        accepted_values = {r['value'] for r in same if r['stage'] == 'opportunity.accepted_at'}
        expiry_values = {r['value'] for r in same if r['stage'] == 'opportunity.expires_at'}
        accepted = next(iter(accepted_values)) if len(accepted_values) == 1 else None
        expires = next(iter(expiry_values)) if len(expiry_values) == 1 else None
        valid_times = (unsigned(accepted, True) and unsigned(expires, True)
                       and expires - accepted == EXPIRY_NS and accepted <= grant['ns'])
        matching_commits = commits.get((pid, grant.get('scope'), grant.get('value')), [])
        valid = (valid_times and all(record_valid(r) for r in same)
                 and all(unsigned(value, True) for value in
                     (pid, identity, grant.get('scope'), grant.get('value')))
                 and len(grants) == len(targets) == 1
                 and type(targets[0].get('value')) is int and 0 < targets[0]['value'] < 1 << 64
                 and len(matching_commits) == 1 and record_valid(matching_commits[0])
                 and matching_commits[0]['ns'] <= accepted)
        lifecycle = {r['stage'] for r in same}
        valid = valid and not any(
            stage in lifecycle for stage in ('opportunity.expired', 'opportunity.revoked', 'opportunity.superseded'))
        valid_enqueue = valid and enqueue and grant['ns'] <= enqueue['ns'] < expires
        frame = attempts.get((pid, enqueue.get('attempt')), []) if enqueue and unsigned(enqueue.get('attempt')) else []
        valid_enqueue = (valid_enqueue and unsigned(enqueue.get('attempt'), True)
                         and unsigned(enqueue.get('presentation'), True) and len(frame) == 1
                         and record_valid(frame[0])
                         and frame[0].get('id') == frame[0].get('attempt')
                         and unsigned(enqueue.get('scope'), True) and unsigned(enqueue.get('value'), True)
                         and frame[0].get('presentation') == enqueue.get('presentation')
                         and grant['ns'] <= frame[0]['ns'] <= enqueue['ns'])
        early = (valid_enqueue and admission and grant['ns'] <= admission['ns'] <= frame[0]['ns']
                 and admission.get('presentation') == enqueue.get('presentation'))
        invalid_lifecycle = len(admissions) > 1 or len(enqueues) > 1
        status = ('revoked' if 'opportunity.revoked' in lifecycle else
                  'expired' if 'opportunity.expired' in lifecycle else
                  'unassigned' if invalid_lifecycle or not valid else
                  'early_enqueued' if early else 'ordinary_enqueued'
                  if valid_enqueue and not admissions else 'unassigned'
                  if enqueue else 'waiting_for_token' if 'opportunity.token_denied' in lifecycle else 'pending')
        rows.append({'opportunity': identity, 'request': grant.get('scope'),
                     'label': grant.get('value'),
                     'target': targets[0].get('value') if len(targets) == 1 else None,
                     'granted_ns': grant['ns'],
                     'accepted_ns': accepted, 'expires_ns': expires,
                     'admitted_ns': admission['ns'] if admission else None,
                     'enqueue_ns': enqueue['ns'] if valid_enqueue else None,
                     'attempt': enqueue.get('attempt') if valid_enqueue else None,
                     'recipient': enqueue.get('scope') if valid_enqueue else None,
                     'projection_revision': enqueue.get('value') if valid_enqueue else None,
                     'status': status, 'output_completed': False})
    return rows


def main():
    import argparse
    import json
    from pathlib import Path
    from latency_report import load_traces, critical_paths
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("samples", type=Path)
    parser.add_argument("--traces", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    run = json.loads(args.samples.read_text())
    records, audit = load_traces(run, args.traces)
    rows = action_opportunities(records)
    paths = critical_paths(run, records, audit)
    args.output.write_text(json.dumps({"opportunities": rows, "critical_paths": paths,
                                      "trace_audit": audit}, indent=2) + "\n")


if __name__ == "__main__":
    main()
