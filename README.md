# PariNS

An ECS-aware caching and filtering DNS forwarder for Pari Public DNS.

PariNS aims to bring client-subnet-aware caching, domain filtering, upstream
selection, and encrypted DNS transports into a single service that is easy to
operate.

## Status

Early development. The Rust project and startup configuration validation are
available; the DNS forwarding service is being implemented.

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
