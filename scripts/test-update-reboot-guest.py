"""Guest-only UP6 fixture. No official download/update is simulated."""
import copy
import hashlib
import http.client
import json
import os
from pathlib import Path
import secrets
import socket
import subprocess
import sys
import time
import traceback

APP = Path('/var/lib/parins-managed')
JOURNAL = Path('/var/lib/parins-updater/private/journal.json')
EVIDENCE = Path('/root/up6-evidence.json')
CREDS = Path('/root/up6-credentials.json')
UNIT = 'parins-managed.service'
SETUP_CONFIG = ('listen = "127.0.0.1:15353"\nquery_timeout_ms = 100\n'
                'tcp_io_timeout_ms = 1000\nshutdown_grace_ms = 1000\n'
                'max_inflight = 16\nmax_tcp_connections = 8\n'
                '[upstreams]\nservers = ["udp://127.0.0.1:9"]\n[filter]\nenabled = true\n'
                'block_exact = ["ci.invalid"]\n[updates]\nauto_check = false\n')


class SetupRejected(Exception):
    def __init__(self, status, value, attempt):
        error = value.get('error') if isinstance(value, dict) else None
        code = error.get('code') if isinstance(error, dict) else None
        allowed = ('update_in_progress', 'INVALID_CONFIG', 'BUSY', 'ALREADY_SETUP',
                   'SETUP_TOKEN', 'BAD_JSON', 'ORIGIN', 'BODY_LIMIT', 'JSON_REQUIRED',
                   'INTERNAL', 'RATE_LIMIT', 'AUTH_BUSY')
        self.diagnostic = {'method': 'POST', 'path': '/api/setup', 'status': status,
                           'expected': 200, 'code': code if code in allowed else 'unknown',
                           'attempt': attempt}
        super().__init__('setup rejected')


def setup(api, credentials, token):
    for attempt in range(1, 4):
        status, value = api.request('POST', '/api/setup', {**credentials, 'toml': SETUP_CONFIG},
                                    {'X-PariNS-Setup': token})
        if status == 200:
            return
        rejected = SetupRejected(status, value, attempt)
        if status != 409 or rejected.diagnostic['code'] != 'update_in_progress' or attempt == 3:
            raise rejected
        time.sleep(1)  # Only an explicit non-accepted rejection may be retried.


def run(*args):
    return subprocess.check_output(args, stderr=subprocess.PIPE, timeout=180).decode().strip()


def property(name, unit=UNIT):
    return run('systemctl', 'show', '--property=' + name, '--value', unit)


def atomic(path, value, owner=None):
    temporary = path.with_name('.up6-' + secrets.token_hex(8))
    with temporary.open('x') as output:
        os.fchmod(output.fileno(), 0o600)
        if owner:
            os.fchown(output.fileno(), owner.st_uid, owner.st_gid)
        json.dump(value, output)
        output.flush()
        os.fsync(output.fileno())
    os.replace(temporary, path)
    directory = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)


def staged_fixture(journal, operation, nonce, invocation, revision, stamp):
    assert journal['operation'] is None and journal['installation_pending'] is None
    result = copy.deepcopy(journal)
    installed = result['installed']
    # Inert identity copied from the actual installation. No candidate file,
    # manifest or official flag is manufactured; only precommit recovery runs.
    result['operation'] = dict(
        status=dict(operation_id=operation, phase_nonce=nonce, phase='staged',
                    version=installed['build']['version'], reason=None, updated_at_ms=stamp,
                    downloaded_bytes=0, total_bytes=0),
        old=installed, candidate=installed, invocation_id=invocation,
        config_revision=revision, started_at_ms=stamp, rollback_attempted=False,
        rollback_started=False, pending_launch=None)
    return result


class API:
    def __init__(self):
        self.auth = {}

    def request(self, method, path, body=None, extra=None):
        conn = http.client.HTTPConnection('127.0.0.1', 3000, timeout=20)
        headers = {'Origin': 'http://127.0.0.1:3000', 'Content-Type': 'application/json', **self.auth, **(extra or {})}
        try:
            conn.request(method, path, None if body is None else json.dumps(body), headers)
            reply = conn.getresponse()
            data = reply.read(1024 * 1024 + 1)
            assert len(data) <= 1024 * 1024
            value = json.loads(data)
            session = value.get('session')
            if reply.status == 200 and isinstance(session, dict) and session.get('binding'):
                self.auth = {'Cookie': reply.getheader('Set-Cookie').split(';')[0],
                             'X-PariNS-Session': session['binding']}
            return reply.status, value
        finally:
            conn.close()

    def ok(self, method, path, body=None):
        status, value = self.request(method, path, body)
        assert status == 200, f'API {method} {path} HTTP {status}'
        return value


def ready():
    api = API()
    deadline = time.monotonic() + 90
    while True:
        try:
            if api.request('GET', '/api/session')[0] == 200:
                break
        except (OSError, http.client.HTTPException):
            pass
        assert time.monotonic() < deadline, 'management readiness deadline'
        time.sleep(1)
    api.ok('POST', '/api/login', json.loads(CREDS.read_text()))
    while True:
        status = api.ok('GET', '/api/status')
        updates = api.ok('GET', '/api/updates')
        if (status['running'] and status['last_error'] is None
                and status['storage']['health'] == 'healthy'
                and updates['frozen'] is False and updates['active_operation'] is None):
            break
        assert time.monotonic() < deadline, 'DNS/storage/update readiness deadline'
        time.sleep(1)
    assert property('DynamicUser') == 'yes'
    pid = property('MainPID')
    assert int(pid) > 1
    assert os.readlink('/proc/' + pid + '/exe') == '/opt/parins-managed/parins'
    uid = int(next(line.split()[1] for line in Path('/proc/' + pid + '/status').read_text().splitlines() if line.startswith('Uid:')))
    assert uid > 0
    query = bytes.fromhex('504e01000001000000000000') + b'\x02ci\x07invalid\x00\x00\x01\x00\x01'
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as dns:
        dns.settimeout(5)
        dns.sendto(query, ('127.0.0.1', 15353))
        answer, peer = dns.recvfrom(4096)
    assert peer == ('127.0.0.1', 15353)
    assert answer[:2] == query[:2] and answer[2] & 128 and answer[3] & 15 == 0
    assert answer[4:6] == b'\x00\x01' and answer[12:len(query)] == query[12:]
    assert answer[6:8] == b'\x00\x00'
    return api, dict(boot_id=Path('/proc/sys/kernel/random/boot_id').read_text().strip(),
                     invocation=property('InvocationID'), uid=uid,
                     recovery_invocation=property('InvocationID', 'parins-update-recovery.service'),
                     elf_sha256=hashlib.sha256(Path('/opt/parins-managed/parins').read_bytes()).hexdigest(),
                     state_sha256=hashlib.sha256((APP / 'state.json').read_bytes()).hexdigest())


def main():
    action, marker = sys.argv[1:]
    assert sys.platform == 'linux' and os.geteuid() == 0
    assert len(marker) == 64 and Path('/etc/parins-up6-fixture').read_text().strip() == marker
    assert Path('/sys/class/dmi/id/product_name').read_text().strip().startswith(('Standard PC', 'QEMU'))
    os.umask(0o077)
    if action == 'install':
        assert not APP.exists() and not JOURNAL.exists()
        # Package members and metadata are verified before the real installer.
        subprocess.run(['sha256sum', '--check', 'SHA256SUMS'], cwd='/home/up6/package',
                       check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=60)
        run('sh', '/home/up6/package/install.sh')
        # Exercise the actual Config parser before sending a setup mutation.
        subprocess.run(['sudo', '-n', '-u', 'up6', '/opt/parins-managed/parins',
                        '--config', '/dev/stdin', '--check'],
                       input=SETUP_CONFIG.encode(), check=True, stdout=subprocess.PIPE,
                       stderr=subprocess.PIPE, timeout=60)
        atomic(CREDS, dict(username='up6-admin', password=secrets.token_hex(24)))
        api = API()
        setup(api, json.loads(CREDS.read_text()), (APP / 'setup-token').read_text().strip())
        _, evidence = ready()
        evidence['installed'] = json.loads(JOURNAL.read_text())['installed']
        assert evidence['installed']['build']['official_release'] is False
        assert evidence['installed']['sha256'] == evidence['elf_sha256']
        atomic(EVIDENCE, evidence)
        print(json.dumps(dict(stage=action, **{k: v for k, v in evidence.items() if k != 'installed'})))
    elif action in ('normal', 'recovered'):
        old = json.loads(EVIDENCE.read_text())
        _, current = ready()
        assert current['boot_id'] != old['boot_id']
        assert current['invocation'] != old['invocation']
        assert current['recovery_invocation'] != old['recovery_invocation']
        for field in ('elf_sha256', 'state_sha256'):
            assert current[field] == old[field]
        journal = json.loads(JOURNAL.read_text())
        assert journal['installed'] == old['installed']
        if action == 'normal':
            assert journal['operation'] is None
            atomic(EVIDENCE, {**old, **current})
        else:
            op = journal['operation']
            assert op['status']['operation_id'] == old['operation_id']
            assert op['status']['phase_nonce'] == old['phase_nonce']
            assert op['status']['phase'] == 'failed' and op['status']['reason'] == 'interrupted'
            assert not op['rollback_attempted'] and not op['rollback_started']
            assert op['pending_launch'] is None
            # A real late Commit is consumed by the unchanged root protocol.
            # Stop only the path watcher so completion has a synchronous owner.
            run('systemctl', 'stop', 'parins-updater.path')
            request = dict(schema=1, operation_id=old['operation_id'], phase_nonce=old['phase_nonce'],
                           request=dict(kind='commit', invocation_id=old['invocation'], config_revision=1))
            atomic(APP / 'update-request.json', request, (APP / 'state.json').stat())
            run('systemctl', 'start', 'parins-updater.service')
            assert not (APP / 'update-request.json').exists()
            assert json.loads(JOURNAL.read_text()) == journal
            assert property('InvocationID') == current['invocation']
            run('systemctl', 'start', 'parins-updater.path')
            ready()
        print(json.dumps(dict(stage=action, **current, precommit_fence=action == 'recovered')))
    elif action == 'stage':
        api, before = ready()
        old = json.loads(EVIDENCE.read_text())
        assert before['boot_id'] == old['boot_id'] and before['invocation'] == old['invocation']
        assert api.ok('GET', '/api/config')['revision'] == 1
        run('systemctl', 'stop', 'parins-updater.path', UNIT, 'parins-updater.service')
        assert property('MainPID') == '0'
        operation, nonce = secrets.token_hex(16), secrets.token_hex(16)
        journal = json.loads(JOURNAL.read_text())
        staged = staged_fixture(journal, operation, nonce, old['invocation'], 1, int(time.time() * 1000))
        atomic(JOURNAL, staged, JOURNAL.stat())
        assert JOURNAL.stat().st_uid == 0 and JOURNAL.stat().st_mode & 0o777 == 0o600
        atomic(EVIDENCE, {**old, 'operation_id': operation, 'phase_nonce': nonce})
        print(json.dumps(dict(stage=action, fixture_phase='staged', candidate_executed=False)))
    else:
        raise AssertionError('unknown guest action')


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        # API bodies/keys/passwords/installer output never leave the guest.
        frames = traceback.extract_tb(error.__traceback__)
        line = next(frame.lineno for frame in reversed(frames) if frame.filename == __file__)
        detail = {'error': type(error).__name__, 'stage': sys.argv[1], 'line': line}
        if isinstance(error, SetupRejected):
            detail['setup_http'] = error.diagnostic
        print(json.dumps(detail))
        sys.exit(1)
