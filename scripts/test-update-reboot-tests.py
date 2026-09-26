"""Pure fixture contract tests; never starts a VM or a service."""
import copy
import importlib.util
import json
from pathlib import Path
import socket
import tempfile
import threading
import unittest
from unittest.mock import Mock, patch


def load(name):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(name + '.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class FixtureTests(unittest.TestCase):
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
