"""Pure fixture contract tests; never starts a VM or a service."""
import copy
import importlib.util
import json
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import tomllib
import unittest
from unittest.mock import Mock, patch


def load(name):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(name + '.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class FixtureTests(unittest.TestCase):
    def test_setup_config_and_rejection_preserve_bounded_mutation_contract(self):
        guest = load('test-update-reboot-guest')
        config = tomllib.loads(guest.SETUP_CONFIG)
        self.assertEqual(config['tcp_io_timeout_ms'], 1000)
        self.assertEqual(config['max_inflight'], 16)
        self.assertEqual(config['max_tcp_connections'], 8)
        private = 'PRIVATE_TOKEN_PASSWORD_RESPONSE'
        api = Mock()
        api.request.return_value = (400, {'error': {'code': 'INVALID_CONFIG', 'message': private}})
        with self.assertRaises(guest.SetupRejected) as caught:
            guest.setup(api, {'password': private}, private)
        api.request.assert_called_once()
        self.assertEqual(caught.exception.diagnostic, {'method': 'POST', 'path': '/api/setup',
                         'status': 400, 'expected': 200, 'code': 'INVALID_CONFIG', 'attempt': 1})
        self.assertNotIn(private, json.dumps(caught.exception.diagnostic))
        self.assertEqual(guest.SetupRejected(400, {'error': {'code': private}}, 1).diagnostic['code'], 'unknown')
        api.reset_mock()
        api.request.return_value = (409, {'error': {'code': 'update_in_progress'}})
        with patch.object(guest.time, 'sleep') as sleep, self.assertRaises(guest.SetupRejected) as caught:
            guest.setup(api, {}, private)
        self.assertEqual(api.request.call_count, 3)
        self.assertEqual(sleep.call_count, 2)
        self.assertEqual(caught.exception.diagnostic['attempt'], 3)
        api.reset_mock()
        api.request.side_effect = TimeoutError()
        with self.assertRaises(TimeoutError):
            guest.setup(api, {}, private)
        api.request.assert_called_once()

    def test_cloud_init_terminal_error_is_not_retried_or_accepted(self):
        host = load('test-update-reboot')
        private = 'PRIVATE_KEY_SEED_CREDENTIAL'
        payload = json.dumps({'status': 'done', 'extended_status': 'degraded done',
            'detail': private, 'init': {'errors': [], 'recoverable_errors': {'WARNING': [
                'cloud config schema: ssh_genkeytypes invalid ' + private], private: [private]}}}).encode()
        for code in (1, 2):
            observation = {}
            with patch.object(host, 'command', side_effect=subprocess.CalledProcessError(
                    code, ['private-command'], output=payload, stderr=private.encode())) as command:
                with self.assertRaises(RuntimeError):
                    host.wait_cloud_init(['ssh'], observation)
                command.assert_called_once()
            self.assertEqual(observation['last_failure']['returncode'], code)
            summary = observation['cloud_init']
            self.assertEqual(summary['extended_status'], 'degraded done')
            self.assertEqual(summary['stages']['init']['categories'], ['schema', 'ssh_genkeytypes'])
            self.assertNotIn(private, json.dumps(observation))
        with patch.object(host, 'command', side_effect=subprocess.CalledProcessError(255, ['ssh'])):
            with self.assertRaises(subprocess.CalledProcessError):
                host.wait_cloud_init(['ssh'], {})
        self.assertEqual(host.cloud_init_summary(private.encode()), {'format': 'invalid_json'})

    def test_boot_diagnostics_never_return_private_command_or_log_text(self):
        host = load('test-update-reboot')
        private = b'PRIVATE_KEY_SEED_CREDENTIAL'
        error = subprocess.CalledProcessError(255, ['secret-command'],
                                             output=private, stderr=b'Permission denied ' + private)
        self.assertEqual(host.probe_failure(error),
                         {'error': 'authentication_rejected', 'returncode': 255})
        self.assertEqual(host.probe_failure(subprocess.CalledProcessError(2, ['cloud-init'],
                         output=private, stderr=private)), {'error': 'command_failed', 'returncode': 2})
        self.assertEqual(host.probe_failure(subprocess.TimeoutExpired(['secret-command'], 8,
                         output=private, stderr=private)), {'error': 'timeout'})
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'serial.log'
            self.assertEqual(host.serial_markers(path), {'present': False})
            path.write_bytes(b'Linux version test\nCloud-init v. test finished at now\n' + private)
            diagnostic = host.serial_markers(path)
            self.assertTrue(diagnostic['kernel_started_seen_in_tail'])
            self.assertTrue(diagnostic['cloud_init_finished_seen_in_tail'])
            self.assertFalse(diagnostic['kernel_panic_seen_in_tail'])
            self.assertNotIn(private.decode(), json.dumps(diagnostic))

    def test_unauthenticated_session_null_is_read_only_and_does_not_create_auth(self):
        guest = load('test-update-reboot-guest')
        response = Mock(status=200)
        response.read.return_value = b'{"session":null,"setup_required":false}'
        connection = Mock()
        connection.getresponse.return_value = response
        with patch.object(guest.http.client, 'HTTPConnection', return_value=connection):
            api = guest.API()
            self.assertEqual(api.request('GET', '/api/session')[0], 200)
            self.assertEqual(api.auth, {})
            response.getheader.assert_not_called()

    def test_qmp_negotiates_and_ignores_events_before_command_reply(self):
        host = load('test-update-reboot')
        observed = []
        with tempfile.TemporaryDirectory(prefix='up6-qmp-test-') as directory:
            path = Path(directory) / 'qmp.sock'
            with socket.socket(socket.AF_UNIX) as listener:
                listener.bind(str(path))
                listener.listen(1)
                listener.settimeout(5)

                def server():
                    conn, _ = listener.accept()
                    with conn, conn.makefile('rwb') as stream:
                        stream.write(b'{"QMP":{}}\n')
                        stream.flush()
                        for reply in ({}, {'running': True}):
                            observed.append(json.loads(stream.readline())['execute'])
                            stream.write(b'{"event":"TEST_EVENT"}\n')
                            stream.write(json.dumps({'return': reply}).encode() + b'\n')
                            stream.flush()

                worker = threading.Thread(target=server)
                worker.start()
                self.assertEqual(host.qmp(path, 'query-status'), {'running': True})
                worker.join(timeout=5)
                self.assertFalse(worker.is_alive())
        self.assertEqual(observed, ['qmp_capabilities', 'query-status'])

    def test_staged_uses_installed_identity_without_claiming_an_upgrade(self):
        guest = load('test-update-reboot-guest')
        anchor = {'sha256': 'a' * 64, 'build': {'version': '0.1.4', 'official_release': False}}
        journal = {'schema': 1, 'installed': anchor, 'operation': None,
                   'last_operation': None, 'installation_pending': None}
        original = copy.deepcopy(journal)
        staged = guest.staged_fixture(journal, 'b' * 32, 'c' * 32, 'd' * 32, 1, 100)
        self.assertEqual(journal, original)
        self.assertEqual(staged['installed'], anchor)
        self.assertEqual(staged['operation']['candidate'], anchor)
        self.assertEqual(staged['operation']['status']['phase'], 'staged')
        self.assertFalse(staged['operation']['candidate']['build']['official_release'])
        journal['operation'] = staged['operation']
        with self.assertRaises(AssertionError):
            guest.staged_fixture(journal, 'b' * 32, 'c' * 32, 'd' * 32, 1, 100)

    def test_host_guard_rejects_local_or_self_hosted(self):
        host = load('test-update-reboot')
        good = dict(GITHUB_ACTIONS='true', RUNNER_ENVIRONMENT='github-hosted', RUNNER_OS='Linux')
        host.guard(good, 'Linux', ['--ephemeral-ci'])
        for environment, system, arguments in [({}, 'Linux', ['--ephemeral-ci']),
                (good, 'Darwin', ['--ephemeral-ci']), (good, 'Linux', []),
                ({**good, 'RUNNER_ENVIRONMENT': 'self-hosted'}, 'Linux', ['--ephemeral-ci'])]:
            with self.assertRaises(AssertionError):
                host.guard(environment, system, arguments)


if __name__ == '__main__':
    unittest.main()
