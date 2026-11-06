#!/usr/bin/env python3
"""Stateful fake gcloud. No networking or fallthrough to an installed SDK."""
import json
import os
from pathlib import Path
import shlex
import sys

args = sys.argv[1:]
root = Path(os.environ['FAKE_CLI_LOG']).parent
with open(os.environ['FAKE_CLI_LOG'], 'a') as log:
    log.write('gcloud\t' + '\t'.join(a.replace('\n', '\\n') for a in args) + '\n')

def flag(name):
    return next(a.split('=', 1)[1] for a in args if a.startswith('--' + name + '='))

def load(name, default):
    path = root / name
    return json.loads(path.read_text()) if path.exists() else default

def save(name, data):
    (root / name).write_text(json.dumps(data))

def reply(data):
    print(json.dumps(data))
    sys.exit(0)

if args[:2] == ['auth', 'print-access-token']:
    print('test-token')
    sys.exit(0)
if not {'--quiet', '--project=test-project'}.issubset(args):
    sys.exit('unexpected unscoped fake request')
if args[0] == 'tasks':
    if args[1:3] == ['queues', 'describe']:
        reply({'state': 'RUNNING', 'retryConfig': {'maxAttempts': -1, 'maxRetryDuration': '0s', 'maxBackoff': '60s'}})
    if args[1] == 'create-http-task':
        task = {'name': 'projects/test-project/locations/us-central1/queues/gpu-expiry/tasks/' + next(a for a in args if a.startswith('gpu-expire-')),
                'scheduleTime': flag('schedule-time'), 'httpRequest': {'url': flag('url'), 'httpMethod': 'DELETE', 'oauthToken': {'serviceAccountEmail': flag('oauth-service-account-email')}}}
        save('gcp-task.json', task)
        reply(task)
    if args[1] == 'describe':
        reply(load('gcp-task.json', {}))
    if args[1] == 'delete':
        (root / 'gcp-task.json').unlink(missing_ok=True)
        reply({})
if args[:3] == ['compute', 'tpus', 'tpu-vm']:
    action, name = args[3:5]
    nodes = load('gcp-nodes.json', {'nodes': []})
    if action == 'list':
        reply([n for n in nodes['nodes'] if f'/locations/{flag("zone")}/' in n['name']])
    if action == 'create':
        assert load('gcp-task.json', {}).get('scheduleTime'), 'paid create without verified expiry'
        metadata = dict(part.split('=', 1) for part in flag('metadata').split('|') if part.startswith(('gpu-workspace=', 'gpu-expiry-queue=')))
        node = {'name': f'projects/test-project/locations/{flag("zone")}/nodes/{name}', 'state': 'CREATING', 'acceleratorType': flag('accelerator-type'), 'metadata': metadata}
        save('gcp-nodes.json', {'nodes': [node]})
        save('gcp-creating-reads.json', 1)
        reply({})
    if action == 'describe':
        if not nodes['nodes']:
            print('NOT_FOUND', file=sys.stderr)
            sys.exit(1)
        node = nodes['nodes'][0]
        remaining = load('gcp-creating-reads.json', 0)
        if remaining:
            save('gcp-creating-reads.json', remaining - 1)
        else:
            node['state'] = 'READY'
            save('gcp-nodes.json', nodes)
        reply(node)
    if action == 'delete':
        save('gcp-nodes.json', {'nodes': []})
        reply({})
    if action == 'ssh':
        key = Path(os.environ['HOME']) / '.ssh/google_compute_engine'
        if '--command=true' in args:
            key.parent.mkdir(parents=True, exist_ok=True)
            key.write_text('FAKE TEST KEY, NOT A CREDENTIAL')
            key.chmod(0o600)
            sys.exit(0)
        if '--dry-run' in args:
            assert key.exists(), 'key provisioning must precede resolved alias'
            print('/usr/bin/ssh -t -i ' + shlex.quote(str(key)) + ' -o CheckHostIP=no -o HostKeyAlias=compute.test -o IdentitiesOnly=yes developer@192.0.2.10')
            sys.exit(0)
sys.exit('unexpected fake request')
