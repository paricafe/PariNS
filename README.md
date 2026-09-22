# PariNS

An ECS-aware caching and filtering DNS forwarder for Pari Public DNS.

PariNS aims to bring client-subnet-aware caching, domain filtering, upstream
selection, and encrypted DNS transports into a single service that is easy to
operate.

## Status

Early design stage. This repository does not yet contain a runnable DNS server.
Build instructions and configuration examples will be added with the implementation.

## Planned scope

- ECS-aware response caching with explicit isolation between upstream profiles.
- Domain filtering with configurable responses, including NOERROR/NODATA.
- Bounded upstream scheduling, connection reuse, and query coalescing.
- UDP/TCP DNS, DNS over TLS, DNS over HTTPS, and DNS over QUIC.
- Operational metrics and atomic configuration and rule updates.

The initial design focuses on forwarding to existing resolvers. A standalone
iterative resolver is outside the initial scope. Rust is the proposed implementation
language; the dependency stack has not yet been finalized.

## Background

PariNS grows out of the operational experience described in
[Pari Public DNS 更新](https://flymc.cc/posts/brand-new-paridns/).

Pari Public DNS is the public service; PariNS is the server software.

## License

Copyright 2026 Natsuki-Kaede and PariNS contributors.

Licensed under the [Apache License, Version 2.0](LICENSE).
