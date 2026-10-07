#!/usr/bin/env python3
"""Conservative public-record joins for bounded terminal opportunities."""

import argparse
import json
from collections import defaultdict
from pathlib import Path
from latency_report import read_process_traces


def _one(values):
    return values[0] if len(values) == 1 else None


def _positive(record, field):
    return type(record.get(field)) is int and 0 < record[field] < 1<<64


class RecordIndex:
    def __init__(self,records):
        self.stage=defaultdict(list)
        self.identity=defaultdict(list)
        self.input=defaultdict(list)
        self.input_opportunity=defaultdict(list)
        self.serialization=defaultdict(list)
        self.outcome=defaultdict(list)
        self.fallback=defaultdict(list)
        self.presentation=defaultdict(list)
        self.queue_occurrence=defaultdict(list)
        self.invalid=set()
        for record in records:
            pid=record.get('pid')
            valid=isinstance(record,dict) and isinstance(record.get('stage'),str) and _positive(record,'pid')
            valid=valid and all(type(value) is int and 0<=value<1<<64 for field,value in record.items() if field!='stage')
            if not valid:
                self.invalid.add(pid)
            self.stage[(pid,record.get('stage'))].append(record)
            self.identity[(pid,record.get('stage'),record.get('id'))].append(record)
            self.input[(pid,record.get('id'),record.get('service'))].append(record)
            self.serialization[(pid,record.get('stage'),record.get('serialization'))].append(record)
            self.presentation[(pid,record.get('stage'),record.get('presentation'))].append(record)
            if record.get('stage') in ('server.enqueue','server.enqueue_full'):
                self.queue_occurrence[(pid,record.get('connection'),record.get('occurrence'))].append(record)
            if record.get('stage')=='opportunity.input':self.input_opportunity[(pid,record.get('value'))].append(record)
            if record.get('stage','').startswith('opportunity.'):self.outcome[(pid,record.get('id'))].append(record['stage'])
            if record.get('stage','').startswith('server.frame_fallback.'):self.fallback[(pid,record.get('attempt'))].append(record)

    def records(self,pid,stage,identity=None):
        return self.stage[(pid,stage)] if identity is None else self.identity[(pid,stage,identity)]


def valid_grant(index,grant):
    return grant is not None and grant.get('pid') not in index.invalid and all(_positive(grant,field) for field in ('pid','id','scope','runtime_instance')) \
        and type(grant.get('value')) is int and 0<=grant['value']<1<<64 and grant['value']%2==0 \
        and len(index.records(grant['pid'],'opportunity.terminal_granted',grant['id']))==1


INPUT_OUTCOMES={'opportunity.input_accepted':'accepted_without_grant','opportunity.input_empty':'empty','opportunity.input_rejected':'rejected'}


def input_status(index,pid,event,service):
    related=index.input[(pid,event,service)]
    outcomes=[record for record in related if record.get('stage') in INPUT_OUTCOMES]
    links=[record for record in related if record.get('stage')=='opportunity.input']
    if pid in index.invalid or type(event) is not int or not 0<event<1<<64 or type(service) is not int or not 0<service<1<<64 or len(outcomes)!=1 or len(links)>1:
        return 'unassigned',None
    outcome=outcomes[0]
    if not links:return INPUT_OUTCOMES[outcome['stage']],None
    link=links[0]
    grant=_one(index.records(pid,'opportunity.terminal_granted',link.get('value')))
    if outcome['stage']!='opportunity.input_accepted' or not valid_grant(index,grant) or not outcome['ns']<=link['ns'] or not grant['ns']<=link['ns']:
        return 'unassigned',None
    return 'granted',link.get('value')


def target_opportunities(records,strict=True):
    """Keep every grant; missing or conflicting identity stays unassigned."""
    rows = []
    index=RecordIndex(records)
    grants = [record for record in records if record.get('stage') == 'opportunity.terminal_granted']
    for grant in grants:
        pid, identity = grant.get('pid'), grant.get('id')
        row = {'pid': pid, 'opportunity': identity, 'status': 'pending',
               'terminal': grant.get('scope'), 'runtime_instance': grant.get('runtime_instance'),
               'baseline_revision': grant.get('value'), 'accepted_ns': grant.get('ns')}
        if not valid_grant(index,grant):
            row['status']='unassigned';rows.append(row);continue
        receipts=index.records(pid,'opportunity.terminal_enqueued',identity)
        if not receipts:
            row['outcomes'] = index.outcome[(pid,identity)]
            pane=_one(index.records(pid,'opportunity.target_pane',identity))
            admissions=index.records(pid,'opportunity.early_terminal_admitted',identity)
            if pane is not None:
                row['attempt_outcomes']=['material_absent' for admission in admissions
                    for event in index.presentation[(pid,'server.target_material_absent',admission.get('presentation'))]
                    if event.get('id')==pane.get('value') and event.get('runtime_instance')==grant.get('runtime_instance')]

            rows.append(row)
            continue
        receipt = _one(receipts)
        inputs = index.input_opportunity[(pid,identity)]
        valid_inputs = bool(inputs) and all(_positive(record,'id') and _positive(record,'service') for record in inputs)
        valid_inputs = valid_inputs and len({(record['id'],record['service']) for record in inputs})==len(inputs)
        event = min(inputs,key=lambda record:record.get('ns',0)) if valid_inputs else None
        pane = _one(index.records(pid,'opportunity.target_pane',identity))
        valid = receipt is not None and event is not None and pane is not None
        if valid:
            valid = (all(_positive(receipt, field) for field in ('runtime_instance', 'presentation', 'attempt', 'serialization', 'scope', 'value'))
                     and _positive(event, 'id') and _positive(event, 'service')
                     and receipt['runtime_instance'] == grant.get('runtime_instance') == pane.get('runtime_instance')
                     and type(grant.get('value')) is int and grant['value'] % 2 == 0
                     and receipt['value'] % 2 == 0 and receipt['value'] > grant['value'])
        if valid:
            for link in inputs:
                status,linked=input_status(index,pid,link['id'],link['service'])
                if status!='granted' or linked!=identity or link['ns']>receipt['ns']:
                    valid=False;break
        surface = enqueue = None
        if valid:
            surface = _one([record for record in index.serialization[(pid,'surface.content',receipt['serialization'])]
                            if record.get('id') == pane.get('value') and record.get('value') == receipt['value']
                            and record.get('runtime_instance') == receipt['runtime_instance']
                            and all(record.get(field) == receipt[field] for field in ('presentation', 'attempt', 'serialization'))])
            if surface is not None:
                enqueue = _one([record for record in index.serialization[(pid,'server.enqueue',receipt['serialization'])]
                                if record.get('id') == surface.get('scope')
                                and all(record.get(field) == receipt[field] for field in ('presentation', 'attempt', 'serialization'))])
            valid = surface is not None and enqueue is not None and all(_positive(enqueue, field) for field in ('connection', 'occurrence'))
        if valid:
            valid = grant['ns'] <= surface['ns'] <= enqueue['ns'] <= receipt['ns']
        if valid:
            attempts=index.queue_occurrence[(pid,enqueue['connection'],enqueue['occurrence'])]
            if len(attempts)!=1 or attempts[0]['stage']!='server.enqueue':valid=False
        if valid and strict:
            expiry=_one(index.records(pid,'opportunity.expires_at',identity))
            begin=_one(index.records(pid,'server.frame_start',receipt['attempt']))
            connection=_one(index.records(pid,'server.connection',enqueue['connection']))
            success=_one([record for record in index.serialization[(pid,'server.target_enqueued',receipt['serialization'])]
                          if record.get('id')==pane['value'] and record.get('scope')==receipt['scope']
                          and record.get('value')==receipt['value'] and record.get('runtime_instance')==receipt['runtime_instance']
                          and record.get('attempt')==receipt['attempt'] and record.get('presentation')==receipt['presentation']])
            failed=index.fallback[(pid,receipt['attempt'])]
            valid=expiry is not None and _positive(expiry,'value') and receipt['ns']<expiry['value']
            valid=valid and begin is not None and begin.get('presentation')==receipt['presentation'] and grant['ns']<=begin['ns']<=surface['ns']
            valid=valid and connection is not None and connection.get('scope')==receipt['scope'] and success is not None and enqueue['ns']<=success['ns']<=receipt['ns'] and connection['ns']<=enqueue['ns'] and not failed
        if not valid:
            row['status'] = 'unassigned'
        else:
            early = index.records(pid,'opportunity.early_terminal_admitted',identity)
            same_presentation=False
            if early:
                if len(early)!=1:row['status']='unassigned';rows.append(row);continue
                same_presentation=early[0].get('presentation')==receipt['presentation']
                prior_starts=index.presentation[(pid,'server.frame_start',early[0].get('presentation'))]
                if not prior_starts or not grant['ns']<=early[0]['ns']<=min(start['ns'] for start in prior_starts)<=receipt['ns']:
                    row['status']='unassigned';rows.append(row);continue
                if not same_presentation:
                    prior_fallback=any(index.fallback[(pid,start.get('attempt'))] for start in prior_starts)
                    prior_residual=bool(index.presentation[(pid,'server.selected_revision_residual',early[0].get('presentation'))])
                    row['prior_admission']={'presentation':early[0]['presentation'],'ns':early[0]['ns'],
                        'outcome':'fallback' if prior_fallback else 'residual' if prior_residual else 'unassigned'}
            row.update(status='early_enqueued' if same_presentation else 'ordinary_enqueued',
                       pane=pane['value'], event=event['id'], service=event['service'], revision=receipt['value'],
                       presentation=receipt['presentation'], attempt=receipt['attempt'], serialization=receipt['serialization'],
                       client=receipt['scope'], connection=enqueue['connection'], occurrence=enqueue['occurrence'],
                       enqueue_ns=enqueue['ns'],inputs=[{'event':record['id'],'service':record['service']} for record in sorted(inputs,key=lambda record:record['ns'])],
                       echo_causality='unavailable')
        rows.append(row)
    return rows


def target_inputs(records):
    """Retain real admission outcomes even if no urgency can be granted."""
    stages=INPUT_OUTCOMES
    rows=[]
    index=RecordIndex(records)
    for record in records:
        if record.get('stage') not in stages: continue
        row={'pid':record.get('pid'),'event':record.get('id'),'service':record.get('service'),
             'status':stages[record['stage']],'ns':record.get('ns')}
        related=index.input[(record.get('pid'),record.get('id'),record.get('service'))]
        status,linked=input_status(index,record.get('pid'),record.get('id'),record.get('service'))
        row['status']=status
        if linked is not None:row['opportunity']=linked
        reason=_one([r for r in related if r.get('stage')=='opportunity.input_no_baseline'])
        if reason is not None: row['reason']=reason['stage']
        rows.append(row)
    return rows


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('traces', nargs='+')
    args = parser.parse_args()
    paths=[Path(path) for path in args.traces]
    directory=paths[0].parent
    if any(path.parent!=directory or not path.stem.isdecimal() for path in paths):
        parser.error('traces must be PID-named JSONL files in one directory')
    expected={int(path.stem):['specified process'] for path in paths}
    records,audit=read_process_traces(directory,expected)
    opportunities=target_opportunities(records,strict=True)
    inputs=target_inputs(records)
    incomplete={process['pid'] for process in audit['processes'] if not process['complete']}
    for row in opportunities+inputs:
        if row['pid'] in incomplete:row['status']='unassigned'
    print(json.dumps({'opportunities':opportunities,'inputs':inputs,'integrity':audit},indent=2))



if __name__ == '__main__':
    main()
