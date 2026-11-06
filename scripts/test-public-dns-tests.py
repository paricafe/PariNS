#!/usr/bin/env python3
"""Offline tests for the public DNS probe's response validation."""

import argparse
import importlib.util
from pathlib import Path
import unittest

import dns.exception
import dns.message
import dns.rcode
import dns.rrset


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


if __name__ == "__main__":
    unittest.main()
