#!/usr/bin/env python3
"""One public-CA-verified DoQ query, without application retries or local DNS."""
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


async def probe(args, result):
    import certifi
    import dns.message
    import dns.rcode
    from aioquic.asyncio import QuicConnectionProtocol
    from aioquic.quic.configuration import QuicConfiguration
    from aioquic.quic.connection import QuicConnection
    from aioquic.quic.events import HandshakeCompleted, ProtocolNegotiated

    result["client"] = {
        name: importlib.metadata.version(name)
        for name in ("aioquic", "dnspython", "certifi")
    }

    class ObservedProtocol(QuicConnectionProtocol):
        def datagram_received(self, data, addr):
            result["received_datagrams"] += 1
            super().datagram_received(data, addr)

        def quic_event_received(self, event):
            if isinstance(event, ProtocolNegotiated):
                result["negotiated_alpn"] = event.alpn_protocol
            elif isinstance(event, HandshakeCompleted):
                result["handshake_completed"] = True
            super().quic_event_received(event)

    configuration = QuicConfiguration(
        is_client=True, alpn_protocols=["doq"], server_name=args.server_name,
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
        result["handshake_started"] = True
        peer = (str(args.address), args.port, 0, 0) if ipv6 else (str(args.address), args.port)
        protocol.connect(peer)
        await protocol.wait_connected()
        require(result.get("negotiated_alpn") == "doq", "wrong ALPN")
        query = dns.message.make_query("example.com.", "A")
        query.id = 0
        wire = query.to_wire()
        reader, writer = await protocol.create_stream()
        writer.write(len(wire).to_bytes(2, "big") + wire)
        writer.write_eof()
        result["dns_query_written"] = True
        length = int.from_bytes(await reader.readexactly(2), "big")
        response = dns.message.from_wire(await reader.readexactly(length))
        require(await reader.read(1) == b"", "extra data after DoQ response frame")
        require(query.is_response(response), "DNS response does not match request")
        result["rcode"] = dns.rcode.to_text(response.rcode())
        result["answer_records"] = sum(len(rrset) for rrset in response.answer)
        require(response.rcode() == dns.rcode.NOERROR and result["answer_records"] > 0,
                "expected successful public DNS resolution")
        result["dns_response_received"] = True
    finally:
        protocol.close()
        transport.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--address", required=True, type=public_address)
    parser.add_argument("--server-name", required=True, type=server_name)
    parser.add_argument("--port", default=853, type=port_number)
    args = parser.parse_args()
    result = {
        "utc": datetime.now(timezone.utc).isoformat(),
        "python": platform.python_version(),
        "address": str(args.address), "port": args.port,
        "family": "IPv6" if args.address.version == 6 else "IPv4",
        "server_name": args.server_name, "offered_alpn": ["doq"],
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
