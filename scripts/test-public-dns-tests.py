#!/usr/bin/env python3
"""Offline validation and loopback transport tests for the public DNS probe."""

import argparse
import asyncio
from datetime import datetime, timedelta, timezone
import importlib.util
import ipaddress
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

import certifi
import dns.exception
import dns.message
import dns.rcode
import dns.rrset
from aioquic.asyncio import QuicConnectionProtocol, serve
from aioquic.h3.connection import H3Connection
from aioquic.h3.events import DataReceived, HeadersReceived
from aioquic.quic.configuration import QuicConfiguration
from aioquic.quic.events import StreamDataReceived
from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.x509.oid import NameOID


spec = importlib.util.spec_from_file_location(
    "public_dns_probe", Path(__file__).with_name("test-public-dns.py")
)
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)


class DnsResponseTests(unittest.TestCase):
    def setUp(self):
        self.query = dns.message.make_query("example.com.", "A", id=0)
        self.response = dns.message.make_response(self.query)
        self.response.answer.append(
            dns.rrset.from_text("example.com.", 60, "IN", "A", "93.184.216.34")
        )

    def test_matching_successful_answer_is_accepted(self):
        response = probe.validate_dns_response(self.query, self.response.to_wire())
        self.assertEqual(response.answer, self.response.answer)

    def test_mismatched_id_and_question_are_rejected(self):
        wrong_id = dns.message.from_wire(self.response.to_wire())
        wrong_id.id = 1
        wrong_question = dns.message.make_response(
            dns.message.make_query("other.example.", "A", id=0)
        )
        wrong_question.answer = self.response.answer
        for response in (wrong_id, wrong_question):
            with self.subTest(response=response), self.assertRaisesRegex(
                ValueError, "does not match request"
            ):
                probe.validate_dns_response(self.query, response.to_wire())

    def test_malformed_and_out_of_bounds_messages_are_rejected(self):
        for wire in (b"", b"\0" * 11, b"\0" * 65536):
            with self.subTest(size=len(wire)), self.assertRaisesRegex(
                ValueError, "invalid DNS response size"
            ):
                probe.validate_dns_response(self.query, wire)
        with self.assertRaises(dns.exception.DNSException):
            probe.validate_dns_response(self.query, self.response.to_wire()[:-1])

    def test_empty_answer_and_error_rcode_are_rejected(self):
        empty = dns.message.make_response(self.query)
        failure = dns.message.from_wire(self.response.to_wire())
        failure.set_rcode(dns.rcode.NXDOMAIN)
        for response in (empty, failure):
            with self.subTest(rcode=response.rcode()), self.assertRaisesRegex(
                ValueError, "expected successful public DNS resolution"
            ):
                probe.validate_dns_response(self.query, response.to_wire())


class HttpHeaderTests(unittest.TestCase):
    def test_accepts_success_and_dns_media_type_with_parameters(self):
        probe.validate_http_headers([
            (b":status", b"200"),
            (b"content-type", b" Application/DNS-Message ; charset=binary"),
        ])

    def test_missing_duplicate_and_wrong_headers_are_rejected(self):
        status = (b":status", b"200")
        media = (b"content-type", b"application/dns-message")
        cases = (
            ([media], "HTTP 200"),
            ([status, status, media], "HTTP 200"),
            ([(b":status", b"500"), media], "HTTP 200"),
            ([status], "DNS media type"),
            ([status, media, media], "DNS media type"),
            ([status, (b"content-type", b"application/json")], "DNS media type"),
        )
        for headers, message in cases:
            with self.subTest(headers=headers), self.assertRaisesRegex(ValueError, message):
                probe.validate_http_headers(headers)

    def test_header_count_and_bytes_are_bounded(self):
        valid = [(b":status", b"200"), (b"content-type", b"application/dns-message")]
        for headers in (valid + [(b"x", b"")] * 127, valid + [(b"x", b"a" * 8192)]):
            with self.subTest(count=len(headers)), self.assertRaisesRegex(
                ValueError, "oversized HTTP response headers"
            ):
                probe.validate_http_headers(headers)


class TargetTests(unittest.TestCase):
    def test_public_address_hostname_and_port_validation(self):
        self.assertEqual(str(probe.public_address("8.8.8.8")), "8.8.8.8")
        self.assertEqual(probe.server_name("DNS.Example.COM"), "dns.example.com")
        self.assertEqual(probe.port_number("443"), 443)
        for parser, values in (
            (probe.public_address, ("127.0.0.1", "224.0.0.1", "localhost")),
            (probe.server_name, ("example.com.", "-bad.example", "a..example")),
            (probe.port_number, ("0", "65536", "not-a-port")),
        ):
            for value in values:
                with self.subTest(value=value), self.assertRaises(argparse.ArgumentTypeError):
                    parser(value)


class LoopbackTransportTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        directory = Path(self.temp.name)
        now = datetime.now(timezone.utc)
        ca_key = ec.generate_private_key(ec.SECP256R1())
        ca_name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "Probe test CA")])
        ca = (
            x509.CertificateBuilder().subject_name(ca_name).issuer_name(ca_name)
            .public_key(ca_key.public_key()).serial_number(x509.random_serial_number())
            .not_valid_before(now - timedelta(minutes=1))
            .not_valid_after(now + timedelta(days=1))
            .add_extension(x509.BasicConstraints(ca=True, path_length=None), critical=True)
            .sign(ca_key, hashes.SHA256())
        )
        server_key = ec.generate_private_key(ec.SECP256R1())
        server_cert = (
            x509.CertificateBuilder()
            .subject_name(x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "dns.example")]))
            .issuer_name(ca_name).public_key(server_key.public_key())
            .serial_number(x509.random_serial_number())
            .not_valid_before(now - timedelta(minutes=1))
            .not_valid_after(now + timedelta(days=1))
            .add_extension(x509.SubjectAlternativeName([x509.DNSName("dns.example")]), critical=False)
            .add_extension(x509.BasicConstraints(ca=False, path_length=None), critical=True)
            .sign(ca_key, hashes.SHA256())
        )
        self.ca_file = directory / "ca.pem"
        cert_file = directory / "server.pem"
        key_file = directory / "server-key.pem"
        self.ca_file.write_bytes(ca.public_bytes(serialization.Encoding.PEM))
        cert_file.write_bytes(server_cert.public_bytes(serialization.Encoding.PEM))
        key_file.write_bytes(server_key.private_bytes(
            serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8,
            serialization.NoEncryption(),
        ))
        self.cert_file = str(cert_file)
        self.key_file = str(key_file)

    async def run_loopback(self, transport_name):
        received = []
        headers_received = []
        tasks = set()

        def start_task(coroutine):
            task = asyncio.create_task(coroutine)
            tasks.add(task)
            task.add_done_callback(tasks.discard)

        def answer(wire):
            query = dns.message.from_wire(wire)
            received.append(query)
            response = dns.message.make_response(query)
            response.answer.append(
                dns.rrset.from_text("example.com.", 60, "IN", "A", "93.184.216.34")
            )
            return response.to_wire()

        class HttpServer(QuicConnectionProtocol):
            def __init__(self, *args, **kwargs):
                super().__init__(*args, **kwargs)
                self.http = H3Connection(self._quic)
                self.requests = {}

            async def respond(self, stream_id, request):
                wire = answer(request)
                self.http.send_headers(stream_id, [
                    (b":status", b"200"),
                    (b"content-type", b"application/dns-message"),
                ])
                self.transmit()
                # Exercise separate response-header and body deliveries.
                await asyncio.sleep(0.05)
                self.http.send_data(stream_id, wire, end_stream=True)
                self.transmit()

            def quic_event_received(self, event):
                for item in self.http.handle_event(event):
                    if isinstance(item, HeadersReceived):
                        headers_received.append(item.headers)
                        self.requests[item.stream_id] = bytearray()
                    elif isinstance(item, DataReceived):
                        self.requests[item.stream_id].extend(item.data)
                        if item.stream_ended:
                            start_task(self.respond(
                                item.stream_id, bytes(self.requests.pop(item.stream_id))
                            ))
                if not isinstance(event, StreamDataReceived):
                    super().quic_event_received(event)

        async def reply_doq(reader, writer):
            length = int.from_bytes(await reader.readexactly(2), "big")
            wire = answer(await reader.readexactly(length))
            writer.write(len(wire).to_bytes(2, "big") + wire)
            writer.write_eof()

        def handle_stream(reader, writer):
            start_task(reply_doq(reader, writer))

        configuration = QuicConfiguration(is_client=False, alpn_protocols=[transport_name])
        configuration.load_cert_chain(self.cert_file, self.key_file)
        server = await serve(
            "127.0.0.1", 0, configuration=configuration,
            create_protocol=HttpServer if transport_name == "h3" else QuicConnectionProtocol,
            stream_handler=handle_stream if transport_name == "doq" else None,
        )
        try:
            args = argparse.Namespace(
                protocol=transport_name, address=ipaddress.ip_address("127.0.0.1"),
                server_name="dns.example", port=server._transport.get_extra_info("sockname")[1],
            )
            result = {"received_datagrams": 0}
            with patch.object(certifi, "where", return_value=str(self.ca_file)):
                await asyncio.wait_for(probe.probe(args, result), timeout=3)
            self.assertEqual(result["stage"], "complete")
            self.assertTrue(result["handshake_completed"])
            self.assertEqual(result["negotiated_alpn"], transport_name)
            self.assertTrue(result["dns_response_received"])
            self.assertEqual(result["answer_records"], 1)
            self.assertEqual(len(received), 1)
            self.assertEqual(received[0].question[0].name.to_text(), "example.com.")
            if transport_name == "h3":
                self.assertEqual(len(headers_received), 1)
                self.assertIn((b":method", b"POST"), headers_received[0])
                self.assertIn((b":path", b"/dns-query"), headers_received[0])
                self.assertIn((b"content-type", b"application/dns-message"), headers_received[0])
        finally:
            server.close()
            for task in tasks:
                task.cancel()
            await asyncio.gather(*tasks, return_exceptions=True)

    async def test_http3_dns_query_succeeds(self):
        unraisable_errors = []
        callback_errors = []
        loop = asyncio.get_running_loop()
        previous_handler = loop.get_exception_handler()
        loop.set_exception_handler(
            lambda _loop, context: callback_errors.append(
                (context.get("message"), repr(context.get("exception")))
            )
        )
        try:
            with patch.object(sys, "unraisablehook", side_effect=lambda error: unraisable_errors.append(
                (error.exc_type.__name__, str(error.exc_value))
            )):
                await self.run_loopback("h3")
        finally:
            loop.set_exception_handler(previous_handler)
        self.assertEqual(unraisable_errors, [])
        self.assertEqual(callback_errors, [])

    async def test_doq_dns_query_succeeds(self):
        await self.run_loopback("doq")


if __name__ == "__main__":
    unittest.main()
