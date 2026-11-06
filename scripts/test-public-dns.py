#!/usr/bin/env python3
"""One public-CA-verified DoQ or HTTP/3 DNS query, without application retries or local DNS."""
import argparse
import asyncio
from datetime import datetime, timezone
import importlib.metadata
import ipaddress
import json
import platform
import re
import signal
import socket
import ssl
import time


def public_address(value):
    try:
        address = ipaddress.ip_address(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError("expected an IPv4 or IPv6 literal") from error
    if not address.is_global or address.is_multicast or address.is_unspecified:
        raise argparse.ArgumentTypeError("expected a public unicast address")
    return address


def server_name(value):
    if len(value) > 253 or not all(
        re.fullmatch(r"[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?", label)
        for label in value.split(".")
    ):
        raise argparse.ArgumentTypeError("expected an ASCII certificate hostname")
    return value.lower()


def port_number(value):
    try:
        port = int(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError("expected a port in 1..65535") from error
    if not 1 <= port <= 65535:
        raise argparse.ArgumentTypeError("expected a port in 1..65535")
    return port


def require(condition, message):
    if not condition:
        raise ValueError(message)


def validate_dns_response(query, wire):
    import dns.message
    import dns.rcode

    require(12 <= len(wire) <= 65535, "invalid DNS response size")
    response = dns.message.from_wire(wire)
    require(query.is_response(response), "DNS response does not match request")
    require(response.rcode() == dns.rcode.NOERROR and bool(response.answer),
            "expected successful public DNS resolution")
    return response


def validate_http_headers(headers):
    require(len(headers) <= 128 and sum(len(k) + len(v) for k, v in headers) <= 8192,
            "oversized HTTP response headers")
    status = [v for k, v in headers if k == b":status"]
    media = [v for k, v in headers if k == b"content-type"]
    require(status == [b"200"], "expected HTTP 200")
    require(len(media) == 1 and media[0].split(b";", 1)[0].strip().lower()
            == b"application/dns-message", "expected DNS media type")


async def probe(args, result):
    import certifi
    import dns.message
    import dns.rcode
    from aioquic.asyncio import QuicConnectionProtocol
    from aioquic.quic.configuration import QuicConfiguration
    from aioquic.quic.connection import QuicConnection
    from aioquic.quic.events import HandshakeCompleted, ProtocolNegotiated
    from aioquic.h3.connection import H3Connection
    from aioquic.h3.events import DataReceived, HeadersReceived

    result["client"] = {
        name: importlib.metadata.version(name)
        for name in ("aioquic", "dnspython", "certifi")
    }

    class ObservedProtocol(QuicConnectionProtocol):
        def __init__(self, *arguments, **keywords):
            super().__init__(*arguments, **keywords)
            self.http = H3Connection(self._quic) if args.protocol == "h3" else None
            self.response = None
            self.stream_id = None
            self.headers = []
            self.body = bytearray()

        def datagram_received(self, data, addr):
            result["received_datagrams"] += 1
            super().datagram_received(data, addr)

        def quic_event_received(self, event):
            if isinstance(event, ProtocolNegotiated):
                result["negotiated_alpn"] = event.alpn_protocol
            elif isinstance(event, HandshakeCompleted):
                result["handshake_completed"] = True
            if self.http is not None:
                for item in self.http.handle_event(event):
                    if item.stream_id != self.stream_id or self.response is None or self.response.done():
                        continue
                    try:
                        if isinstance(item, HeadersReceived):
                            self.headers.extend(item.headers)
                            validate_http_headers(self.headers)
                        elif isinstance(item, DataReceived):
                            require(len(self.body) + len(item.data) <= 65535,
                                    "oversized DNS response")
                            self.body.extend(item.data)
                        if isinstance(item, (DataReceived, HeadersReceived)) and item.stream_ended:
                            validate_http_headers(self.headers)
                            self.response.set_result(bytes(self.body))
                    except ValueError as error:
                        self.response.set_exception(error)
            super().quic_event_received(event)

    configuration = QuicConfiguration(
        is_client=True, alpn_protocols=[args.protocol], server_name=args.server_name,
        verify_mode=ssl.CERT_REQUIRED, idle_timeout=5,
    )
    configuration.load_verify_locations(cafile=certifi.where())
    ipv6 = args.address.version == 6
    transport, protocol = await asyncio.get_running_loop().create_datagram_endpoint(
        lambda: ObservedProtocol(QuicConnection(configuration=configuration)),
        local_addr=("::" if ipv6 else "0.0.0.0", 0),
        family=socket.AF_INET6 if ipv6 else socket.AF_INET,
    )
    try:
        started = time.monotonic()
        result["stage"] = "handshake"
        result["handshake_started"] = True
        peer = (str(args.address), args.port, 0, 0) if ipv6 else (str(args.address), args.port)
        protocol.connect(peer)
        await protocol.wait_connected()
        result["handshake_ms"] = round((time.monotonic() - started) * 1000, 3)
        require(result.get("negotiated_alpn") == args.protocol, "wrong ALPN")
        query = dns.message.make_query("example.com.", "A")
        query.id = 0
        wire = query.to_wire()
        result["stage"] = "query"
        query_started = time.monotonic()
        if args.protocol == "doq":
            reader, writer = await protocol.create_stream()
            writer.write(len(wire).to_bytes(2, "big") + wire)
            writer.write_eof()
            result["dns_query_written"] = True
            length = int.from_bytes(await reader.readexactly(2), "big")
            require(12 <= length <= 65535, "invalid DoQ response frame")
            answer = await reader.readexactly(length)
            require(await reader.read(1) == b"", "extra data after DoQ response frame")
        else:
            protocol.response = asyncio.get_running_loop().create_future()
            protocol.stream_id = protocol._quic.get_next_available_stream_id()
            authority = args.server_name + ("" if args.port == 443 else ":" + str(args.port))
            protocol.http.send_headers(protocol.stream_id, [
                (b":method", b"POST"), (b":scheme", b"https"),
                (b":authority", authority.encode("ascii")), (b":path", b"/dns-query"),
                (b"content-type", b"application/dns-message"),
                (b"accept", b"application/dns-message"),
            ])
            protocol.http.send_data(protocol.stream_id, wire, end_stream=True)
            protocol.transmit()
            result["dns_query_written"] = True
            answer = await protocol.response
            result["http_status"] = 200
        result["dns_response_ms"] = round((time.monotonic() - query_started) * 1000, 3)
        result["stage"] = "validate"
        response = validate_dns_response(query, answer)
        result["rcode"] = dns.rcode.to_text(response.rcode())
        result["answer_records"] = sum(len(rrset) for rrset in response.answer)
        result["dns_response_received"] = True
        result["stage"] = "complete"
    finally:
        protocol.close()
        transport.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--address", required=True, type=public_address)
    parser.add_argument("--server-name", required=True, type=server_name)
    parser.add_argument("--protocol", choices=("doq", "h3"), default="doq")
    parser.add_argument("--port", type=port_number)
    args = parser.parse_args()
    if args.port is None:
        args.port = 853 if args.protocol == "doq" else 443
    result = {
        "utc": datetime.now(timezone.utc).isoformat(),
        "python": platform.python_version(),
        "address": str(args.address), "port": args.port,
        "family": "IPv6" if args.address.version == 6 else "IPv4",
        "server_name": args.server_name, "offered_alpn": [args.protocol],
        "protocol": args.protocol, "stage": "initialize",
        "verify_mode": "CERT_REQUIRED", "ca_source": "certifi public roots",
        "deadline_seconds": 5, "query": "example.com. A",
        "handshake_started": False, "handshake_completed": False,
        "dns_query_written": False, "dns_response_received": False,
        "received_datagrams": 0, "outcome": "failure",
    }

    def hard_deadline(number, frame):
        raise TimeoutError("probe process exceeded safety deadline")

    signal.signal(signal.SIGALRM, hard_deadline)
    signal.alarm(8)
    started = time.monotonic()
    try:
        asyncio.run(asyncio.wait_for(probe(args, result), timeout=5))
        result["outcome"] = "passed"
    except Exception as error:
        result["error_type"] = type(error).__name__
    finally:
        signal.alarm(0)
        result["elapsed_seconds"] = round(time.monotonic() - started, 3)
        print(json.dumps(result, sort_keys=True))
    return 0 if result["outcome"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
