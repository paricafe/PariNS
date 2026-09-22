# PariNS

An ECS-aware caching and filtering DNS forwarder for Pari Public DNS.

PariNS aims to bring client-subnet-aware caching, domain filtering, upstream
selection, and encrypted DNS transports into a single service that is easy to
operate.

## Status

Early development. UDP/TCP listeners, validated single-upstream forwarding,
UDP-to-TCP upstream fallback, bounded connections, and graceful shutdown are
implemented. ECS-aware caching, filtering, and encrypted transports remain planned.

## Development

Install Rust with [rustup](https://rust-lang.org/tools/install/). The repository
pins its toolchain in `rust-toolchain.toml`.

```sh
cargo run -- --config parins.example.toml --check
cargo test --locked
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
```

The example uses loopback addresses and an upstream on port 5354. Set `upstream`
to your chosen resolver's IP and port. Unknown configuration keys and invalid
resource limits are rejected at startup.

## Run

```sh
cargo build --locked --release
cp parins.example.toml parins.toml
# Edit parins.toml and set upstream to your chosen resolver's IP:port.
./target/release/parins --check
./target/release/parins
```

In another terminal:

```sh
dig @127.0.0.1 -p 5353 example.com A
dig @127.0.0.1 -p 5353 example.com A +tcp
```

Use `--config PATH` to select a different configuration file. `listen` selects
the same address and port for UDP and TCP; port zero selects a shared ephemeral
port, printed on startup. `upstream` must be a literal IP:port, reachable over
both UDP and TCP. Do not point it back at PariNS, including through a local
interface alias that configuration validation cannot identify.

Ctrl-C or SIGTERM (Unix) stops accepting traffic, closes idle TCP clients, and
allows active queries up to `shutdown_grace_ms` to finish before cancellation.

## Behavior and limits

- One upstream UDP transaction per query, with an independent socket and random
  ID. Only responses matching the upstream endpoint, ID, opcode, and question
  are accepted. Upstream truncation triggers TCP fallback under the same
  `query_timeout_ms` deadline. Failures return SERVFAIL.
- UDP replies respect the client's payload limit, capped at 1232 bytes (512
  without EDNS). Larger replies return a question-only TC response; retry over
  TCP for the complete answer. Downstream TCP connections support sequential
  reuse; requests on the same connection are processed in order.
- `max_inflight` is shared across UDP and TCP queries. At capacity, new UDP
  queries are dropped and valid TCP queries receive SERVFAIL. Excess TCP
  connections are closed. `tcp_io_timeout_ms` bounds each complete frame read
  or write, including idle and partial-frame reads.
- Only ordinary single-question QUERY messages are supported. AXFR, IXFR, ANY,
  and signed queries are refused; other opcodes return NOTIMP. Malformed queries
  with a readable DNS header return FORMERR; unreadable packets and response
  packets sent to the listener are dropped (TCP connections are closed).
- EDNS/DO/CD are forwarded without generating ECS or caching answers. This
  release does not perform DNSSEC validation or authenticate the plaintext
  upstream; the AD bit is cleared in client responses.

The default is local-only. This first increment is not a production public
resolver: encrypted transports, per-client rate limiting, operational metrics,
and production capacity validation are not yet implemented. Logs contain startup
and shutdown events, not query names or client IP addresses.

## Module boundaries

| Module | Responsibility |
| --- | --- |
| `config` | Parse and validate startup configuration |
| `protocol` | DNS parsing, request rules, response correlation, UDP encoding |
| `resolver` | Shared query path, total upstream deadline, SERVFAIL and AD policy |
| `upstream` | Independent UDP exchange and validated TCP fallback |
| `transport::tcp` | Length-prefixed framing used on both sides |
| `server` | Listener ownership, admission budgets, client tasks, shutdown |
| `main` | CLI arguments, startup, OS signals |

Tests use controlled loopback upstreams and do not rely on public DNS answers.

## Planned scope

- ECS-aware response caching with explicit isolation between upstream profiles.
- Domain filtering with configurable responses, including NOERROR/NODATA.
- Bounded upstream scheduling, connection reuse, and query coalescing.
- UDP/TCP DNS, DNS over TLS, DNS over HTTPS, and DNS over QUIC.
- Operational metrics and atomic configuration and rule updates.

The initial design focuses on forwarding to existing resolvers. A standalone
iterative resolver is outside the initial scope. The implementation uses Rust,
Tokio for asynchronous IO, and Hickory for DNS message parsing and encoding.

## Background

PariNS grows out of the operational experience described in
[Pari Public DNS 更新](https://flymc.cc/posts/brand-new-paridns/).

Pari Public DNS is the public service; PariNS is the server software.

## License

Copyright 2026 Natsuki-Kaede and PariNS contributors.

Licensed under the [Apache License, Version 2.0](LICENSE).
