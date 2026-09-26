"""Disposable hosted-runner QEMU test; the runner itself is never rebooted."""
import hashlib
import json
import os
from pathlib import Path
import platform
import secrets
import signal
import shutil
import socket
import subprocess
import sys
import tempfile
import time

IMAGE = 'https://cloud-images.ubuntu.com/releases/noble/release-20260911/ubuntu-24.04-server-cloudimg-amd64.img'
IMAGE_SHA = '612b2c0cc1bc413a6cb8c38fd611794caf0f2b436c50013d8b3794db12ad7354'


def guard(env, system, args):
    assert args[:1] == ['--ephemeral-ci'] and system == 'Linux'
    assert env.get('GITHUB_ACTIONS') == 'true' and env.get('RUNNER_OS') == 'Linux'
    assert env.get('RUNNER_ENVIRONMENT') == 'github-hosted'


def command(args, **kwargs):
    return subprocess.check_output(args, stderr=subprocess.PIPE, timeout=kwargs.pop('timeout', 60), **kwargs)


def probe_failure(error):
    """Classify captured stderr without returning any captured text or arguments."""
    if isinstance(error, subprocess.TimeoutExpired):
        return {'error': 'timeout'}
    detail = {'error': 'command_failed', 'returncode': error.returncode}
    stderr = error.stderr or b''
    for marker, category in ((b'Connection refused', 'connection_refused'),
                             (b'Connection timed out', 'connection_timeout'),
                             (b'Host key verification failed', 'host_key_rejected'),
                             (b'REMOTE HOST IDENTIFICATION HAS CHANGED', 'host_key_rejected'),
                             (b'Permission denied', 'authentication_rejected'),
                             (b'Connection reset', 'connection_reset')):
        if marker in stderr:
            detail['error'] = category
            break
    return detail


def serial_markers(path):
    # Only fixed booleans leave the private log. Never publish raw serial text:
    # cloud-init can print identities and user data when its own setup fails.
    if not path.exists():
        return {'present': False}
    size = path.stat().st_size
    with path.open('rb') as stream:
        stream.seek(max(0, size - 131072))
        tail = stream.read(131072)
    return {'present': True, 'bytes': size,
            'kernel_started_seen_in_tail': b'Linux version ' in tail,
            'kernel_panic_seen_in_tail': b'Kernel panic' in tail,
            'cloud_init_started_seen_in_tail': b'Cloud-init v.' in tail,
            'cloud_init_finished_seen_in_tail': b'Cloud-init v.' in tail and b' finished at ' in tail,
            'login_prompt_seen_in_tail': b'parins-up6 login:' in tail}


def qmp(path, operation):
    with socket.socket(socket.AF_UNIX) as sock:
        sock.settimeout(5)
        sock.connect(str(path))
        stream = sock.makefile('rwb')
        assert 'QMP' in json.loads(stream.readline(65536))
        for name in ('qmp_capabilities', operation):
            stream.write(json.dumps({'execute': name}).encode() + b'\n')
            stream.flush()
            while True:
                reply = json.loads(stream.readline(65536))
                if 'event' not in reply:
                    assert 'return' in reply
                    break
        return reply['return']


def main():
    guard(os.environ, platform.system(), sys.argv[1:])
    def interrupted(_signum, _frame):
        raise InterruptedError('fixture cancelled')
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    assert len(sys.argv) == 4 and sys.argv[2] == '--package'
    package = Path(sys.argv[3]).resolve(strict=True)
    assert package.is_dir() and (package / 'install-build-info.json').is_file()
    build = json.loads((package / 'install-build-info.json').read_text())
    assert build['official_release'] is False and build['source_commit'] == os.environ['GITHUB_SHA']
    assert build['target'] == 'x86_64-unknown-linux-musl'
    for tool in ('curl', 'qemu-system-x86_64', 'qemu-img', 'cloud-localds', 'ssh', 'scp', 'ssh-keygen'):
        assert shutil.which(tool), 'missing prerequisite: ' + tool
    repo = Path(__file__).resolve().parent.parent
    guest_script = (repo / 'scripts/test-update-reboot-guest.py').read_bytes()
    result_dir = Path(os.environ['RUNNER_TEMP']) / 'parins-up6-results'
    result_dir.mkdir(mode=0o700)
    result = dict(result='running', scope='development-build boot/precommit recovery only',
                  source_commit=os.environ['GITHUB_SHA'], image_url=IMAGE, image_sha256=IMAGE_SHA,
                  accelerator='tcg', vcpus=2, memory_mib=2048, stages=[])

    def save():
        (result_dir / 'result.json').write_text(json.dumps(result, indent=2) + '\n')

    save()
    vm = None
    stage = 'prepare'
    os.umask(0o077)
    # All disks, private keys, cloud-init seed and raw logs belong to this exact
    # temporary directory. It is removed only after our Popen child has exited.
    with tempfile.TemporaryDirectory(prefix='parins-up6-') as directory:
        root = Path(directory)
        try:
            command(['curl', '--fail', '--silent', '--show-error', '--location', '--proto', '=https',
                     '--proto-redir', '=https', '--connect-timeout', '15', '--max-time', '600',
                     IMAGE, '-o', str(root / 'base.img')], timeout=610)
            with (root / 'base.img').open('rb') as image:
                actual = hashlib.file_digest(image, 'sha256').hexdigest()
            assert actual == IMAGE_SHA, 'cloud image digest mismatch'
            command(['qemu-img', 'create', '-f', 'qcow2', '-F', 'qcow2', '-b', str(root / 'base.img'),
                     str(root / 'disk.qcow2'), '10G'])
            for name in ('client', 'host'):
                command(['ssh-keygen', '-q', '-t', 'ed25519', '-N', '', '-f', str(root / name)])
            marker = secrets.token_hex(32)
            cloud = {'users': [{'name': 'up6', 'sudo': 'ALL=(ALL) NOPASSWD:ALL',
                                'shell': '/bin/bash', 'lock_passwd': True,
                                'ssh_authorized_keys': [(root / 'client.pub').read_text().strip()]}],
                     'ssh_pwauth': False, 'disable_root': True,
                     'ssh_keys': {'ed25519_private': (root / 'host').read_text(),
                                  'ed25519_public': (root / 'host.pub').read_text().strip()},
                     'ssh_genkeytypes': [], 'ssh_deletekeys': False,
                     'write_files': [{'path': '/etc/parins-up6-fixture', 'owner': 'root:root',
                                      'permissions': '0600', 'content': marker + '\n'}]}
            (root / 'user-data').write_text('#cloud-config\n' + json.dumps(cloud))
            (root / 'meta-data').write_text(json.dumps({'instance-id': 'parins-up6-' + secrets.token_hex(8),
                                                       'local-hostname': 'parins-up6'}))
            command(['cloud-localds', str(root / 'seed.img'), str(root / 'user-data'), str(root / 'meta-data')])
            with socket.socket() as reserve:
                reserve.bind(('127.0.0.1', 0))
                port = reserve.getsockname()[1]
            # The only TCP listener forwarded by QEMU is SSH on loopback. DNS
            # and the console are checked from inside the guest over loopback.
            hostkey = ' '.join((root / 'host.pub').read_text().split()[:2])
            (root / 'known_hosts').write_text(f'[127.0.0.1]:{port} {hostkey}\n')
            options = ['-i', str(root / 'client'), '-o', 'IdentitiesOnly=yes', '-o', 'BatchMode=yes',
                       '-o', 'StrictHostKeyChecking=yes', '-o', 'GlobalKnownHostsFile=/dev/null',
                       '-o', 'UserKnownHostsFile=' + str(root / 'known_hosts'), '-o', 'ConnectTimeout=3']
            ssh = ['ssh', *options, '-p', str(port), 'up6@127.0.0.1']
            with (root / 'qemu.log').open('wb') as log:
                vm = subprocess.Popen(['qemu-system-x86_64', '-machine', 'q35,accel=tcg', '-cpu', 'max',
                    '-smp', '2', '-m', '2048', '-display', 'none', '-monitor', 'none',
                    '-serial', 'file:' + str(root / 'serial.log'),
                    '-qmp', 'unix:' + str(root / 'qmp.sock') + ',server=on,wait=off',
                    '-drive', 'file=' + str(root / 'disk.qcow2') + ',if=virtio,format=qcow2',
                    '-drive', 'file=' + str(root / 'seed.img') + ',if=virtio,format=raw,readonly=on',
                    '-netdev', f'user,id=net0,hostfwd=tcp:127.0.0.1:{port}-:22',
                    '-device', 'virtio-net-pci,netdev=net0'], stdout=log, stderr=log)

            def wait_boot(previous=None):
                deadline = time.monotonic() + 300
                observation = {'stage': stage, 'attempts': 0, 'boot_observed': False}
                result['boot_wait'] = observation
                while time.monotonic() < deadline:
                    observation['attempts'] += 1
                    observation['probe'] = 'ssh_boot_id'
                    assert vm.poll() is None, 'owned QEMU exited'
                    try:
                        boot = command(ssh + ['cat', '/proc/sys/kernel/random/boot_id'], timeout=8).decode().strip()
                        observation.pop('last_failure', None)
                        observation['boot_observed'] = True
                        observation['boot_changed'] = previous is None or boot != previous
                        if previous is None or boot != previous:
                            observation['probe'] = 'cloud_init'
                            command(ssh + ['sudo', '-n', 'cloud-init', 'status', '--wait'], timeout=120)
                            observation['ready'] = True
                            observation.pop('last_failure', None)
                            return boot
                    except (subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
                        observation['last_failure'] = probe_failure(error)
                        # Read-only readiness probes only, never replay mutations.
                    time.sleep(2)
                raise TimeoutError('guest boot readiness')

            def guest(action):
                try:
                    payload = command(ssh + ['sudo', '-n', 'python3', '-', action, marker],
                                      input=guest_script, timeout=300)
                except subprocess.CalledProcessError as error:
                    # Preserve only fixed diagnostic fields, never subprocess
                    # stderr or response bodies that might carry credentials.
                    try:
                        detail = json.loads(error.output)
                        if detail.get('stage') == action and isinstance(detail.get('line'), int):
                            result['guest_error'] = {'stage': action, 'line': detail['line']}
                    except (ValueError, AttributeError):
                        pass
                    raise
                observation = json.loads(payload)
                assert observation.get('stage') == action
                result['stages'].append(observation)
                save()
                return observation

            def reboot(previous):
                # One request; even a disconnected SSH result is resolved only
                # by a changed boot_id, never by issuing a second reboot.
                attempt = subprocess.run(ssh + ['sudo', '-n', 'systemctl', 'reboot'],
                                         stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=20)
                assert attempt.returncode in (0, 255)
                return wait_boot(previous)

            stage = 'first-boot'
            wait_boot()
            assert qmp(root / 'qmp.sock', 'query-status')['running'] is True
            command(['scp', *options, '-P', str(port), '-r', str(package), 'up6@127.0.0.1:/home/up6/package'], timeout=120)
            stage = 'install'
            installed = guest('install')
            stage = 'normal-reboot'
            reboot(installed['boot_id'])
            normal = guest('normal')
            stage = 'stage'
            guest('stage')
            stage = 'precommit-reboot'
            reboot(normal['boot_id'])
            guest('recovered')
            result['result'] = 'passed'
        except Exception as error:
            result.update(result='failed', failed_stage=stage, error_type=type(error).__name__)
            if vm is not None:
                result['qemu_returncode'] = vm.poll()
                try:
                    status = qmp(root / 'qmp.sock', 'query-status')
                    result['qemu_running'] = status.get('running') is True
                    allowed = ('running', 'paused', 'shutdown', 'prelaunch', 'internal-error',
                               'io-error', 'guest-panicked', 'watchdog')
                    result['qemu_status'] = status.get('status') if status.get('status') in allowed else 'other'
                except Exception as diagnostic_error:
                    result['qmp_error_type'] = type(diagnostic_error).__name__
                result['serial_markers'] = serial_markers(root / 'serial.log')
            raise
        finally:
            signal.signal(signal.SIGTERM, signal.SIG_IGN)
            signal.signal(signal.SIGINT, signal.SIG_IGN)
            if vm is not None and vm.poll() is None:
                try:
                    qmp(root / 'qmp.sock', 'quit')
                    vm.wait(timeout=10)
                except Exception:
                    vm.terminate()
                    try:
                        vm.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        vm.kill()
                        vm.wait(timeout=10)
            save()
            if result['result'] == 'failed':
                print(json.dumps(result), flush=True)
    print(json.dumps(result))


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        # Never echo subprocess stderr, cloud-init data, auth or private paths.
        print(json.dumps({'result': 'failed', 'error_type': type(error).__name__}))
        sys.exit(1)
