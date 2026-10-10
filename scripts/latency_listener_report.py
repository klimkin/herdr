#!/usr/bin/env python3
"""Report local attach stages; retain completions with unavailable attribution."""
import argparse
from collections import Counter
import json
from pathlib import Path

from latency_report import distribution, process_trace_complete, read_process_traces, wait_service, runtime_work


def listener_report(run, records, trace_audit=None):
    """Native peer IDs prove one connection only when both populations are unique."""
    rows = []
    integer = lambda value: type(value) is int and 0 <= value < 1 << 64
    valid = all(isinstance(item, dict) and isinstance(item.get('stage'), str)
                and all(integer(item.get(field)) for field in ('pid','ns','id','value','scope'))
                and item['pid'] > 0
                and all(integer(item.get(field, 0)) for field in ('dropped','connection','occurrence','offset'))
                for item in records)
    if not valid:
        records = []
    client_pids = run.get('client_pids', [])
    server_pid = run.get('server_pid')
    lifetime_valid = (type(server_pid) is int and server_pid > 0
                      and isinstance(client_pids, list)
                      and all(type(pid) is int and pid > 0 for pid in client_pids)
                      and len(set(client_pids)) == len(client_pids)
                      and server_pid not in client_pids)
    population = Counter(row.get('client_pid') for row in run.get('attaches', []))
    for attach in run.get('attaches', []):
        pid, server = attach.get('client_pid'), run.get('server_pid')
        row = {**attach, 'status': 'unassigned', 'connect_to_accept_ns': None,
               'connect_to_welcome_ns': None, 'welcome_return_to_received_ns': None, 'accept_to_hello_ns': None,
               'hello_to_welcome_ns': None, 'welcome_to_output_ns': None,
               'output_to_observed_ns': None, 'launch_to_observed_ns': None}
        launch, observed = attach.get('launch_ns'), attach.get('observed_ns')
        if integer(launch) and integer(observed) and launch <= observed:
            row['launch_to_observed_ns'] = observed - launch

        def unique(stage, process, identifier=None):
            values = [item for item in records if item['stage'] == stage
                      and item['pid'] == process and (identifier is None or item['id'] == identifier)]
            return values[0] if len(values) == 1 else None

        metadata_valid = (type(pid) is int and pid > 0 and type(server) is int and server > 0
                          and integer(attach.get('launch_ns')) and integer(attach.get('intended_ns'))
                          and (observed is None or integer(observed)))
        accepted = [item for item in records if item['stage'] == 'client.accepted'
                    and item['pid'] == server and item['value'] == pid]
        begin = unique('client.connect_begin', pid)
        connected = unique('client.connect_complete', pid)
        if connected and begin and connected['id'] != begin['id']:
            connected = None
        hello_begin = unique('client.hello_begin', pid)
        sent = unique('client.hello_written', pid)
        if sent and hello_begin and sent['id'] != hello_begin['id']:
            sent = None
        received = unique('client.welcome_received', pid)
        if received and hello_begin and received['id'] != hello_begin['id']:
            received = None
        client_connection = unique('client.connection', pid)
        schemas = [unique('diagnostics.schema', process) for process in (pid, server)]
        schema_valid = all(schema and schema['value'] == 2 for schema in schemas)
        if (not valid or not metadata_valid or not lifetime_valid or pid not in client_pids or not schema_valid):
            row['reason'] = 'Invalid PID or timestamp population'
        elif (population[pid] != 1 or len(accepted) != 1 or not begin or not connected
                or not hello_begin or not sent or not received or not client_connection):
            row['reason'] = 'Missing or ambiguous single-process connection/handshake population'
        elif not all(process_trace_complete(records, process, trace_audit) for process in (pid, server)):
            row['reason'] = 'Incomplete diagnostics; zero drops alone cannot prove fidelity'
        else:
            accept = accepted[0]
            handler = unique('client.handshake_begin', server, accept['id'])
            hello = unique('client.hello_ready', server, accept['id'])
            welcome = unique('client.welcome_written', server, accept['id'])
            connections = [item for item in records if item['stage'] == 'server.connection'
                           and item['pid'] == server and item['value'] == pid]
            failures = [item for item in records if item['pid'] == pid
                        and item['stage'] in ('client.connect_failed', 'client.hello_failed', 'client.welcome_failed')]
            chain = [begin, accept, handler, hello, received, client_connection]
            if (failures or not all(chain) or not welcome or len(connections) != 1
                    or any(item['id'] <= 0 for item in chain + [welcome, connected, hello_begin, sent, *connections])
                    or connections[0]['scope'] != accept['id']
                    or connections[0]['ns'] < welcome['ns']
                    or begin['ns'] < launch
                    or begin['id'] == hello_begin['id']
                    or welcome['ns'] < hello['ns']
                    or connections[0]['value'] != pid or connected['value'] != server
                    or client_connection['value'] != server
                    or any(first['ns'] > last['ns'] for first, last in zip(chain, chain[1:]))
                    or not begin['ns'] <= connected['ns'] <= hello_begin['ns'] <= sent['ns'] <= received['ns']):
                row['reason'] = 'Missing, incompatible or unordered connection boundaries'
            else:
                outputs = [item for item in records if item['pid'] == pid
                           and item['stage'] == 'client.output_complete' and item['value'] > 0
                           and received['ns'] <= item['ns']]
                output = min(outputs, key=lambda item:item['ns'], default=None)
                row.update(status='measured', client_id=accept['id'],
                           connection=connections[0]['id'], client_connection=client_connection['id'],
                           connect_submit_ns=begin['ns'], connect_complete_ns=connected['ns'],
                           accept_ns=accept['ns'], handshake_begin_ns=handler['ns'],
                           hello_ready_ns=hello['ns'], welcome_written_ns=welcome['ns'],
                           welcome_received_ns=received['ns'], hello_submit_ns=hello_begin['ns'],
                           hello_complete_ns=sent['ns'], connect_to_accept_ns=accept['ns']-begin['ns'],
                           connect_to_welcome_ns=received['ns']-begin['ns'],
                           accept_to_hello_ns=hello['ns']-accept['ns'],
                           hello_to_welcome_ns=welcome['ns']-hello['ns'],
                           first_output_ns=output['ns'] if output else None,
                           welcome_return_to_received_ns=received['ns']-welcome['ns'] if welcome['ns'] <= received['ns'] else None)
                if welcome['ns'] > received['ns']:
                    row['welcome_return_reason'] = 'Peer received complete frame before server write-return timestamp'
                if output and type(observed) is int and output['ns'] <= observed:
                    row.update(welcome_to_output_ns=output['ns']-received['ns'],
                               output_to_observed_ns=observed-output['ns'])
                else:
                    row['output_reason'] = 'First nonempty output unavailable or flush returned after outer observation'
        artifacts = [item for item in run.get('artifact_status', []) if item.get('observer_pid') == pid]
        raw_complete = False
        if len(artifacts) == 1 and pid in client_pids:
            artifact = artifacts[0]
            sink = artifact.get('sink', {})
            observed_bytes = artifact.get('observer_bytes')
            raw_complete = (artifact.get('read_complete') is True and artifact.get('reader_error') is None
                            and type(artifact.get('output_index')) is int
                            and artifact['output_index'] == client_pids.index(pid)
                            and artifact.get('raw_file') == f"client-{artifact['output_index']}.vt"
                            and sink.get('complete') is True
                            and integer(observed_bytes) and integer(sink.get('accepted_bytes'))
                            and integer(sink.get('persisted_bytes'))
                            and observed_bytes == sink['accepted_bytes'] == sink['persisted_bytes']
                            and sink.get('dropped_batches') == 0 and sink.get('error') is None
                            and artifact.get('raw_file_valid') is True)
        row['raw_output_fidelity'] = 'complete' if raw_complete else 'incomplete_or_unavailable'
        rows.append(row)
    columns = ('connect_to_accept', 'connect_to_welcome', 'welcome_return_to_received', 'accept_to_hello',
               'hello_to_welcome', 'welcome_to_output', 'output_to_observed', 'launch_to_observed')
    return {'schema':'herdr.listener-report.v1', 'attaches':rows,
            'summary':{name:distribution([row[name+'_ns'] for row in rows if row[name+'_ns'] is not None],len(rows))
                       for name in columns},
            'population':{'offered':len(rows), 'validated_outer_completed':sum(row['launch_to_observed_ns'] is not None and row['raw_output_fidelity']=='complete' for row in rows),
                          'outer_completed':sum(row['launch_to_observed_ns'] is not None for row in rows),
                          'uniquely_attributed':sum(row['status']=='measured' for row in rows),
                          'accept_attributed_outer_completed':sum(row['status']=='measured' and row['launch_to_observed_ns'] is not None for row in rows)},
            'wait_service':wait_service(records), 'runtime_work':runtime_work(records),
            'limits':['Current baseline already contains readiness; no incremental W optimization.',
                      'First output is nonempty TUI output and can be startup/warning work; only the outer marker proves required committed surface.',
                      'Connect return and hello write return can overlap server handling; stage quantiles are not additive.',
                      'Loop returns are not scheduler wakeups; CPU/off-CPU and physical display latency unavailable.',
                      'No historical polling comparator: causal readiness CPU difference unavailable.',
                      'Linux observations do not validate native macOS/Windows.']}


def process_output_report(responses):
    """Helper write start is independent of controller command/ACK latency."""
    results=[]
    integer=lambda value:type(value) is int and 0<=value<1<<64
    counts=Counter(row.get('identity') for row in responses if isinstance(row,dict) and isinstance(row.get('identity'),str))
    for row in responses:
        if not isinstance(row,dict):
            continue
        if row.get('kind')!='output':
            continue
        result={**row,'process_to_observed_ns':None,'write_return_to_observed_ns':None,
                'controller_to_observed_ns':None}
        start,end,observed=row.get('process_write_start_ns'),row.get('process_write_end_ns'),row.get('observed_ns')
        submitted=row.get('submitted_ns')
        clock=row.get('clock_bracket_ns',[])
        if integer(submitted) and integer(observed) and submitted<=observed:
            result['controller_to_observed_ns']=observed-submitted
        if (isinstance(row.get('identity'),str) and bool(row['identity']) and counts[row['identity']]==1
                and integer(submitted) and row.get('helper_reply_identity')==row['identity']
                and isinstance(clock,list) and len(clock)==3 and all(integer(value) for value in clock) and clock==sorted(clock)
                and integer(start) and integer(end) and integer(observed)
                and 0<clock[0]==submitted<=start<=end<=clock[2] and start<=observed<=clock[2]):
            result['process_to_observed_ns']=observed-start
            if end<=observed:
                result['write_return_to_observed_ns']=observed-end
        results.append(result)
    return {'responses':results,'limits':['Process start includes helper write and server/client path; controller command scheduling excluded.',
                                         'Peer observation can precede helper write-return; affected interval unavailable.']}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('samples',type=Path)
    parser.add_argument('--output',type=Path,required=True)
    args = parser.parse_args()
    run = json.loads(args.samples.read_text())
    for artifact in run.get('artifact_status', []):
        name = artifact.get('raw_file')
        path = args.samples.parent / name if isinstance(name, str) and Path(name).name == name else None
        artifact['raw_file_valid'] = (path is not None and path.is_file()
                                      and path.stat().st_size == artifact.get('observer_bytes'))
    expected = {run['server_pid']:['server']}
    expected.update({pid:['client'] for pid in run['client_pids']})
    # stderr/PTY evidence catches bounded flush warnings beyond JSONL records.
    diagnostics = {run['server_pid']:[args.samples.parent/'server.stderr']}
    for index, pid in enumerate(run['client_pids']):
        diagnostics[pid] = [args.samples.parent/f'client-{index}.vt']
    records, audit = read_process_traces(args.samples.parent/'traces', expected, diagnostics)
    report = listener_report(run, records, audit)
    report['trace_integrity'] = audit
    report['process_output'] = process_output_report(run.get('responses',[]))
    args.output.write_text(json.dumps(report,indent=2)+'\n')
    print(json.dumps(report['population']))


if __name__ == '__main__':
    main()
