#!/usr/bin/env python3
"""External-file certificate reload, only in the owned disposable systemd fixture."""
import argparse
import grp
import hashlib
import http.cookiejar
import json
import os
from pathlib import Path
import platform
import signal
import socket
import ssl
import stat
import struct
import subprocess
import time
import urllib.error
import urllib.request


UNIT = "parins-managed.service"
GROUP = "parins-cert-ci"
EXTERNAL = Path("/var/lib/parins-cert-ci")
DROP_DIR = Path("/etc/systemd/system/parins-managed.service.d")
DROP_IN = DROP_DIR / "91-ci-certificate-renewal.conf"
UNIT_MARKER = "# PariNS managed installer unit v3 (restricted updater contract)"
LOCAL_FILES = (
    "ca.pem", "ca-key.pem", "leaf.ext", "a.pem", "a-key.pem", "a.csr",
    "b.pem", "b-key.pem", "b.csr", "drop-in.conf",
)


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def command(*args, root=False, timeout=15):
    argv = (["sudo", "-n"] if root else []) + [str(arg) for arg in args]
    result = subprocess.run(argv, capture_output=True, timeout=timeout, check=False)
    # Never expose command output: credentials and generated key material stay private.
    require(result.returncode == 0, f"{Path(str(args[0])).name} failed ({result.returncode})")
    return result.stdout


def property_value(name):
    return command("systemctl", "show", UNIT, f"--property={name}", "--value").decode().strip()


def service_identity():
    pid = property_value("MainPID")
    require(pid.isdigit() and int(pid) > 1, "expected live fixture service")
    require(property_value("DynamicUser") == "yes", "expected DynamicUser fixture")
    require(property_value("ActiveState") == "active", "fixture service is not active")
    require(command("readlink", f"/proc/{pid}/exe", root=True).decode().strip()
            == "/opt/parins-managed/parins", "fixture executable changed")
    invocation = property_value("InvocationID")
    require(bool(invocation), "missing service invocation")
    return pid, invocation


def guarded_fixture(path):
    require(os.environ.get("GITHUB_ACTIONS") == "true"
            and os.environ.get("RUNNER_ENVIRONMENT") == "github-hosted"
            and os.environ.get("RUNNER_OS") == "Linux" and platform.system() == "Linux",
            "requires disposable GitHub-hosted Linux")
    require(os.geteuid() != 0, "run as fixture owner, not root")
    resolved = path.resolve(strict=True)
    require(path == resolved and path.name.startswith("parins-systemd-test."),
            "expected canonical systemd fixture directory")
    info = path.lstat()
    require(stat.S_ISDIR(info.st_mode) and info.st_uid == os.geteuid()
            and stat.S_IMODE(info.st_mode) == 0o700, "fixture must be owner-only")
    for name in ("credentials.json", "state-copy"):
        info = (path / name).lstat()
        require(stat.S_ISREG(info.st_mode) and info.st_uid == os.geteuid()
                and stat.S_IMODE(info.st_mode) == 0o600, "missing private fixture evidence")
    unit = command("cat", "/etc/systemd/system/" + UNIT, root=True).decode()
    require(UNIT_MARKER in unit.splitlines(), "not the owned installer fixture unit")
    service_identity()
    require(property_value("CanReload") == "yes", "fixture unit cannot reload")
    reload = property_value("ExecReload")
    require("argv[]=/bin/kill -HUP $MAINPID ; ignore_errors=no" in reload,
            "unexpected ExecReload contract")


class Api:
    def __init__(self):
        self.binding = None
        self.opener = urllib.request.build_opener(
            urllib.request.ProxyHandler({}),
            urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar()),
        )

    def request(self, method, path, body=None):
        headers = {"Origin": "http://127.0.0.1:3000"}
        if self.binding:
            headers["X-PariNS-Session"] = self.binding
        if body is not None:
            headers["Content-Type"] = "application/json"
        request = urllib.request.Request(
            "http://127.0.0.1:3000" + path,
            data=None if body is None else json.dumps(body).encode(),
            method=method, headers=headers,
        )
        try:
            with self.opener.open(request, timeout=5) as response:
                require(response.status == 200, "unexpected API status")
                return json.load(response)
        except urllib.error.HTTPError as error:
            # No body/headers: login responses carry a session binding.
            raise RuntimeError(f"{method} {path}: HTTP {error.code}") from None


def poll(read, predicate, description, seconds=20):
    deadline = time.monotonic() + seconds
    while True:
        value = read()
        if predicate(value):
            return value
        require(time.monotonic() < deadline, f"timed out waiting for {description}")
        time.sleep(0.1)


def public_fingerprint(path):
    return hashlib.sha256(command("openssl", "x509", "-in", path, "-outform", "DER")).hexdigest()


def fresh_tls(port, ca, expected):
    context = ssl.create_default_context(cafile=str(ca))
    context.set_alpn_protocols(["dot"])
    with socket.create_connection(("127.0.0.1", port), timeout=3) as raw:
        with context.wrap_socket(raw, server_hostname="dns.test") as client:
            require(client.selected_alpn_protocol() == "dot", "wrong DoT ALPN")
            require(hashlib.sha256(client.getpeercert(binary_form=True)).hexdigest() == expected,
                    "fresh verified TLS fingerprint mismatch")
            # The fixture's existing local rule blocks this name without upstream IO.
            question = b"\x02ci\x07invalid\x00\x00\x01\x00\x01"
            query = struct.pack("!6H", 0x504E, 0x100, 1, 0, 0, 0) + question
            deadline = time.monotonic() + 3
            client.sendall(struct.pack("!H", len(query)) + query)

            def read_exact(size):
                data = b""
                while len(data) < size:
                    remaining = deadline - time.monotonic()
                    require(remaining > 0, "DoT DNS response exceeded deadline")
                    client.settimeout(remaining)
                    block = client.recv(size - len(data))
                    require(bool(block), "unexpected DoT DNS EOF")
                    data += block
                return data

            response = read_exact(struct.unpack("!H", read_exact(2))[0])
            require(len(response) >= 12 + len(question), "short DoT DNS response")
            ident, flags, questions, answers, _, _ = struct.unpack("!6H", response[:12])
            require(ident == 0x504E and flags & 0x8000 and flags & 0xF == 0
                    and questions == 1 and answers == 0
                    and response[12:12 + len(question)] == question,
                    "DoT DNS query/response mismatch")


def stable(before, after):
    for name in ("revision", "generation", "listen"):
        require(after[name] == before[name], f"reload changed {name}")
    require(after["dns_health"]["generation"] == before["dns_health"]["generation"],
            "reload changed DNS health generation")
    require(after["running"] is True and after["last_error"] is None, "DNS not healthy after reload")


def exercise(fixture):
    guarded_fixture(fixture)
    # Each mutation has one owner; no repeated API apply or reload on unknown outcomes.
    require(not EXTERNAL.exists() and not EXTERNAL.is_symlink(), "external fixture path exists")
    require(not DROP_IN.exists() and not DROP_IN.is_symlink(), "fixture drop-in exists")
    try:
        grp.getgrnam(GROUP)
    except KeyError:
        pass
    else:
        raise RuntimeError("fixture group exists")
    require(not DROP_DIR.is_symlink(), "drop-in directory must not be a symlink")
    local = fixture / "certificate-renewal"
    local.mkdir(mode=0o700)
    made_group = made_external = made_dropin = made_dropdir = False
    external_inode = None
    group_gid = None
    try:
        command("openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-sha256",
                "-days", "2", "-subj", "/CN=PariNS isolated fixture CA", "-addext",
                "basicConstraints=critical,CA:TRUE", "-addext", "keyUsage=critical,keyCertSign,cRLSign",
                "-keyout", local / "ca-key.pem", "-out", local / "ca.pem")
        (local / "leaf.ext").write_text(
            "subjectAltName=DNS:dns.test\nbasicConstraints=critical,CA:FALSE\n"
            "keyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n"
        )
        for number, leaf in enumerate(("a", "b"), 1):
            command("openssl", "req", "-new", "-newkey", "rsa:2048", "-nodes", "-subj",
                    "/CN=dns.test", "-keyout", local / f"{leaf}-key.pem", "-out", local / f"{leaf}.csr")
            command("openssl", "x509", "-req", "-sha256", "-days", "2", "-in", local / f"{leaf}.csr",
                    "-CA", local / "ca.pem", "-CAkey", local / "ca-key.pem", "-set_serial", str(number),
                    "-extfile", local / "leaf.ext", "-out", local / f"{leaf}.pem")
        first, second = (public_fingerprint(local / f"{leaf}.pem") for leaf in ("a", "b"))
        require(first != second, "fixture identities must differ")
        command("groupadd", "--system", GROUP, root=True)
        made_group = True
        group_gid = grp.getgrnam(GROUP).gr_gid
        command("mkdir", "-m", "0750", EXTERNAL, root=True)
        made_external = True
        command("chown", f"root:{GROUP}", EXTERNAL, root=True)
        external_inode = command("stat", "-c", "%d:%i", EXTERNAL, root=True).strip()

        def replace(leaf, *, key_only=False):
            for source, target in ((f"{leaf}.pem", "cert"), (f"{leaf}-key.pem", "key")):
                if key_only and target != "key":
                    continue
                command("install", "-o", "root", "-g", GROUP, "-m", "0640",
                        local / source, EXTERNAL / f"{target}.next", root=True)
                command("mv", "-T", EXTERNAL / f"{target}.next", EXTERNAL / f"{target}.pem", root=True)

        replace("a")
        if not DROP_DIR.exists():
            command("mkdir", "-m", "0755", DROP_DIR, root=True)
            made_dropdir = True
        (local / "drop-in.conf").write_text(f"# PariNS isolated certificate fixture\n[Service]\nSupplementaryGroups={GROUP}\n")
        command("install", "-o", "root", "-g", "root", "-m", "0644", local / "drop-in.conf", DROP_IN, root=True)
        made_dropin = True
        command("systemctl", "daemon-reload", root=True)
        command("systemctl", "restart", UNIT, root=True, timeout=160)
        api = Api()

        def ready():
            try:
                return api.request("GET", "/api/session")
            except (urllib.error.URLError, TimeoutError, ConnectionError):
                return None

        poll(ready, lambda value: value is not None and value["setup_required"] is False,
             "management after group setup")
        identity = service_identity()
        proc = command("cat", f"/proc/{identity[0]}/status", root=True).decode().splitlines()
        groups = next(line.split()[1:] for line in proc if line.startswith("Groups:"))
        require(str(group_gid) in groups, "running DynamicUser lacks external certificate group")
        credentials = json.loads((fixture / "credentials.json").read_text())
        api.binding = api.request("POST", "/api/login", credentials)["session"]["binding"]
        poll(lambda: api.request("GET", "/api/updates"),
             lambda value: value["frozen"] is False and value["active_operation"] is None,
             "update reconciliation")
        original = api.request("GET", "/api/config")
        require("[dot]" not in original["toml"] and "[doh]" not in original["toml"],
                "expected existing plaintext fixture configuration")
        with socket.socket() as reservation:
            reservation.bind(("127.0.0.1", 0))
            port = reservation.getsockname()[1]
        candidate = original["toml"] + (
            f'\n[dot]\nlisten = "127.0.0.1:{port}"\n'
            f'cert_file = "{EXTERNAL}/cert.pem"\nkey_file = "{EXTERNAL}/key.pem"\n'
        )
        applied = api.request("PUT", "/api/config", {"revision": original["revision"], "toml": candidate})
        require(applied["revision"] == original["revision"] + 1, "configuration apply revision mismatch")
        before = api.request("GET", "/api/status")
        saved = api.request("GET", "/api/config")
        require(saved["toml"] == candidate and saved["revision"] == applied["revision"],
                "saved fixture configuration mismatch")
        require(before["running"] is True, "DoT apply did not start DNS")
        require(service_identity() == identity, "configuration apply restarted the process")
        fresh_tls(port, local / "ca.pem", first)
        generation = before["certificates"]["certificate_generation"]
        require(len(before["certificates"]["roles"]) == 1
                and before["certificates"]["roles"][0]["role"] == "dot", "expected exactly one DoT identity")
        require(before["certificates"]["roles"][0]["leaf_sha256"] == first, "status initial leaf mismatch")

        def reload_expect(outcome, fingerprint, expected_generation):
            previous = api.request("GET", "/api/status")["certificates"]["last_reload"]
            attempt = 0 if previous is None else previous["attempt_id"]
            command("systemctl", "reload", UNIT, root=True)
            after = poll(lambda: api.request("GET", "/api/status"),
                         lambda value: value["certificates"]["last_reload"] is not None
                         and value["certificates"]["last_reload"]["attempt_id"] > attempt,
                         "certificate reload terminal status")
            summary = after["certificates"]
            require(summary["last_reload"]["source"] == "signal"
                    and summary["last_reload"]["outcome"] == outcome, "unexpected reload terminal outcome")
            require(summary["last_reload"]["error_code"] == ("CERTIFICATE_INVALID" if outcome == "failed" else None),
                    "unexpected reload error classification")
            require(summary["certificate_generation"] == expected_generation, "certificate generation mismatch")
            require(summary["roles"][0]["leaf_sha256"] == fingerprint, "status leaf mismatch")
            stable(before, after)
            require(api.request("GET", "/api/config") == saved, "reload changed saved configuration")
            require(service_identity() == identity, "reload changed process identity")
            fresh_tls(port, local / "ca.pem", fingerprint)
            print(json.dumps({"stage": "certificate-renewal", "outcome": outcome,
                              "certificate_generation": expected_generation,
                              "leaf_sha256": fingerprint, "fresh_verified_tls": True,
                              "dot_dns_nodata": True,
                              "revision": after["revision"], "dns_generation": after["generation"]}))

        replace("b")
        reload_expect("applied", second, generation + 1)
        replace("a", key_only=True)
        reload_expect("failed", second, generation + 1)
        replace("b", key_only=True)
        reload_expect("unchanged", second, generation + 1)
        directory_mode = command("stat", "-c", "%u:%g:%a", EXTERNAL, root=True).decode().strip()
        require(directory_mode == f"0:{group_gid}:750", "external certificate directory ownership changed")
        for name in ("cert.pem", "key.pem"):
            actual = command("stat", "-c", "%u:%g:%a", EXTERNAL / name, root=True).decode().strip()
            require(actual == f"0:{group_gid}:640", "external certificate ownership changed")
        print("Linux DynamicUser external-group certificate rotation and failed-reload preservation passed.")
    finally:
        # Stop before removing any loaded identity, group or drop-in. Existing FS7
        # cleanup runs outside this fixture and remains unchanged.
        if made_group or made_external or made_dropin:
            command("systemctl", "stop", UNIT, root=True, timeout=160)
            require(property_value("MainPID") == "0", "refusing cleanup while service remains alive")
        if made_dropin:
            require(command("cat", DROP_IN, root=True) == (local / "drop-in.conf").read_bytes(),
                    "fixture drop-in changed; refusing deletion")
            command("rm", "--", DROP_IN, root=True)
        if made_dropdir:
            command("rmdir", DROP_DIR, root=True)
        if made_external:
            require(external_inode is not None and command("stat", "-c", "%d:%i", EXTERNAL, root=True).strip()
                    == external_inode, "external fixture directory changed; refusing cleanup")
            for name in ("cert.pem", "key.pem", "cert.next", "key.next"):
                command("rm", "-f", "--", EXTERNAL / name, root=True)
            command("rmdir", EXTERNAL, root=True)
        if made_group:
            require(grp.getgrnam(GROUP).gr_gid == group_gid, "fixture group changed; refusing cleanup")
            command("groupdel", GROUP, root=True)
        if made_dropin:
            command("systemctl", "daemon-reload", root=True)
        for name in LOCAL_FILES:
            (local / name).unlink(missing_ok=True)
        local.rmdir()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ephemeral-ci", required=True, action="store_true")
    parser.add_argument("fixture", type=Path)
    args = parser.parse_args()
    os.umask(0o077)
    def interrupted(number, frame):
        raise SystemExit(128 + number)

    for sig in (signal.SIGHUP, signal.SIGINT, signal.SIGTERM):
        signal.signal(sig, interrupted)
    exercise(args.fixture)


if __name__ == "__main__":
    main()
