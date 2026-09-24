//! ECS provenance, validation, and response scope. No IO or cache ownership.

use std::net::IpAddr;

use anyhow::{Result, ensure};
use hickory_proto::{
    op::{Edns, Header, Message, Query, ResponseCode},
    rr::{
        DNSClass, Name, RecordType,
        rdata::opt::{ClientSubnet, EdnsCode, EdnsOption},
    },
    serialize::binary::{BinDecodable, BinDecoder},
};
use ipnet::IpNet;

use crate::{config::EcsConfig, protocol::MAX_UDP_PAYLOAD};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    NoEcs,
    Privacy {
        ipv4: bool,
    },
    Network(IpNet),
    /// Missing upstream ECS is reusable only for this exact sent source prefix.
    ExactSource(IpNet),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResponseScope {
    pub cache: Option<Scope>,
    pub reply: Option<u8>,
}

#[derive(Clone)]
pub struct Context {
    pub outgoing: Option<ClientSubnet>,
    incoming: Option<ClientSubnet>,
}

pub fn subnet(message: &Message) -> Option<ClientSubnet> {
    match message.edns.as_ref()?.option(EdnsCode::Subnet)? {
        EdnsOption::Subnet(subnet) => Some(*subnet),
        _ => None,
    }
}

pub fn set_subnet(message: &mut Message, subnet: Option<ClientSubnet>) {
    if let Some(edns) = &mut message.edns {
        edns.options_mut().remove(EdnsCode::Subnet);
    }
    if let Some(subnet) = subnet {
        let edns = message.edns.get_or_insert_with(|| {
            let mut edns = Edns::new();
            edns.set_max_payload(MAX_UDP_PAYLOAD);
            edns
        });
        edns.options_mut().insert(EdnsOption::Subnet(subnet));
    }
}

impl Context {
    pub fn prepare(
        query: &Message,
        peer: IpAddr,
        config: &EcsConfig,
    ) -> std::result::Result<(Message, Self), ResponseCode> {
        let incoming = subnet(query);
        if incoming.is_some_and(|ecs| ecs.scope_prefix() != 0) {
            return Err(ResponseCode::FormErr);
        }
        let mut outgoing_query = query.clone();
        if !config.enabled || query.queries[0].query_class() != DNSClass::IN {
            set_subnet(&mut outgoing_query, None);
            return Ok((
                outgoing_query,
                Self {
                    incoming: None,
                    outgoing: None,
                },
            ));
        }
        // IPv4-mapped IPv6 socket peers carry IPv4 client identity.
        let peer = match peer {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(peer, IpAddr::V4),
            _ => peer,
        };
        let (address, source) = if let Some(ecs) = incoming {
            let network =
                IpNet::new(ecs.addr(), ecs.source_prefix()).map_err(|_| ResponseCode::FormErr)?;
            if ecs.source_prefix() > 0 && !network.contains(&peer) {
                return Err(ResponseCode::Refused);
            }
            let cap = if ecs.addr().is_ipv4() {
                config.ipv4_prefix
            } else {
                config.ipv6_prefix
            };
            (ecs.addr(), ecs.source_prefix().min(cap))
        } else {
            (
                peer,
                if peer.is_ipv4() {
                    config.ipv4_prefix
                } else {
                    config.ipv6_prefix
                },
            )
        };
        let network = IpNet::new(address, source)
            .map_err(|_| ResponseCode::FormErr)?
            .trunc();
        let outgoing = ClientSubnet::new(network.addr(), source, 0);
        set_subnet(&mut outgoing_query, Some(outgoing));
        Ok((
            outgoing_query,
            Self {
                incoming,
                outgoing: Some(outgoing),
            },
        ))
    }

    /// Storage and downstream scopes come from the same response decision.
    /// The caller still excludes the original namespace after a privacy retry.
    pub fn response_scope(&self, response: &Message) -> ResponseScope {
        let received = subnet(response);
        let cache = match (self.outgoing, received) {
            (None, None) => Some(Scope::NoEcs),
            (Some(sent), None) if sent.source_prefix() == 0 => Some(Scope::Privacy {
                ipv4: sent.addr().is_ipv4(),
            }),
            (Some(sent), None) => IpNet::new(sent.addr(), sent.source_prefix())
                .ok()
                .map(|network| Scope::ExactSource(network.trunc())),
            (Some(sent), Some(received))
                if received.addr() == sent.addr()
                    && received.source_prefix() == sent.source_prefix()
                    && received.scope_prefix() <= sent.source_prefix() =>
            {
                if sent.source_prefix() == 0 {
                    Some(Scope::Privacy {
                        ipv4: sent.addr().is_ipv4(),
                    })
                } else {
                    IpNet::new(sent.addr(), received.scope_prefix())
                        .ok()
                        .map(|network| Scope::Network(network.trunc()))
                }
            }
            _ => None,
        };
        let reply = cache
            .and_then(Scope::reply_scope)
            .or_else(|| received.map(|ecs| ecs.scope_prefix()));
        ResponseScope { cache, reply }
    }

    /// Rebuild ECS for the original caller, not the cache-filling client.
    pub fn finish(&self, query: &Message, response: &mut Message, scope: Option<u8>) {
        response.metadata.id = query.id;
        response.metadata.recursion_desired = query.recursion_desired;
        response.metadata.checking_disabled = query.checking_disabled;
        response.metadata.authentic_data = false;
        response.queries = query.queries.clone();
        set_subnet(response, None);
        if let Some(client_edns) = &query.edns {
            let edns = response.edns.get_or_insert_with(Edns::new);
            edns.set_max_payload(MAX_UDP_PAYLOAD)
                .set_dnssec_ok(client_edns.flags().dnssec_ok);
            // An unsolicited upstream COOKIE belongs to its transaction, not
            // to any of the downstream clients sharing the answer (RFC 7873).
            let cookie = EdnsCode::from(10);
            if client_edns.option(cookie).is_none() {
                edns.options_mut().remove(cookie);
            }
        } else {
            response.edns = None;
        }
        if let Some(original) = self.incoming {
            let echoed = ClientSubnet::new(
                original.addr(),
                original.source_prefix(),
                scope.unwrap_or(original.source_prefix()),
            );
            set_subnet(response, Some(echoed));
        }
    }
}

pub fn response_matches(query: &Message, response: &Message) -> bool {
    match (subnet(query), subnet(response)) {
        (_, None) => true,
        (Some(sent), Some(received)) => {
            sent.addr() == received.addr() && sent.source_prefix() == received.source_prefix()
        }
        (None, Some(_)) => false,
    }
}

impl Scope {
    pub fn matches(self, outgoing: Option<ClientSubnet>) -> bool {
        match (self, outgoing) {
            (Self::NoEcs, None) => true,
            (Self::Privacy { ipv4 }, Some(ecs)) => {
                ecs.source_prefix() == 0 && ipv4 == ecs.addr().is_ipv4()
            }
            (Self::Network(network), Some(ecs)) => {
                ecs.source_prefix() > 0
                    && network.prefix_len() <= ecs.source_prefix()
                    && network.contains(&ecs.addr())
            }
            (Self::ExactSource(network), Some(ecs)) => {
                ecs.source_prefix() > 0
                    && network.prefix_len() == ecs.source_prefix()
                    && IpNet::new(ecs.addr(), ecs.source_prefix())
                        .is_ok_and(|sent| sent.trunc() == network)
            }
            _ => false,
        }
    }

    pub fn prefix_len(self) -> u8 {
        match self {
            Self::Network(network) | Self::ExactSource(network) => network.prefix_len(),
            _ => 0,
        }
    }

    pub fn reply_scope(self) -> Option<u8> {
        match self {
            Self::NoEcs => None,
            _ => Some(self.prefix_len()),
        }
    }

    pub fn tag(self) -> String {
        match self {
            Self::NoEcs => "no_ecs".into(),
            Self::Privacy { ipv4: true } => "privacy_v4".into(),
            Self::Privacy { ipv4: false } => "privacy_v6".into(),
            Self::Network(network) => network.to_string(),
            Self::ExactSource(network) => format!("exact_ecs:{network}"),
        }
    }

    pub fn parse_tag(text: &str) -> Result<Self> {
        Ok(match text {
            "no_ecs" => Self::NoEcs,
            "privacy_v4" => Self::Privacy { ipv4: true },
            "privacy_v6" => Self::Privacy { ipv4: false },
            _ => {
                if let Some(network) = text.strip_prefix("exact_ecs:") {
                    let parsed: IpNet = network.parse()?;
                    ensure!(
                        parsed.prefix_len() > 0
                            && parsed == parsed.trunc()
                            && network == parsed.to_string(),
                        "exact ECS scope requires a nonzero canonical CIDR"
                    );
                    Self::ExactSource(parsed)
                } else {
                    Self::Network(text.parse::<IpNet>()?.trunc())
                }
            }
        })
    }
}

/// Hickory decodes ECS into typed fields but does not reject extra ADDRESS bytes.
/// Walk RR envelopes with Hickory's decoder to validate the original OPT lengths.
pub fn validate_wire(bytes: &[u8]) -> Result<()> {
    let mut decoder = BinDecoder::new(bytes);
    let header = Header::read(&mut decoder)?;
    for _ in 0..header.counts.queries {
        Query::read(&mut decoder)?;
    }
    let records = u32::from(header.counts.answers)
        + u32::from(header.counts.authorities)
        + u32::from(header.counts.additionals);
    let mut ecs_count = 0;
    let mut padding_count = 0;
    for _ in 0..records {
        Name::read(&mut decoder)?;
        let kind = decoder.read_u16()?.unverified();
        decoder.read_u16()?;
        decoder.read_u32()?;
        let length = decoder.read_u16()?.unverified() as usize;
        let data = decoder.read_slice(length)?.unverified();
        if kind != u16::from(RecordType::OPT) {
            continue;
        }
        let mut options = BinDecoder::new(data);
        while !options.is_empty() {
            let code = options.read_u16()?.unverified();
            let length = options.read_u16()?.unverified() as usize;
            let data = options.read_slice(length)?.unverified();
            if code == 12 {
                padding_count += 1;
                ensure!(padding_count == 1, "duplicate EDNS Padding");
            }
            if code != 8 {
                continue;
            }
            ecs_count += 1;
            ensure!(
                ecs_count == 1 && data.len() >= 4,
                "invalid or duplicate ECS"
            );
            let family = u16::from_be_bytes([data[0], data[1]]);
            let bits = match family {
                1 => 32,
                2 => 128,
                _ => anyhow::bail!("invalid ECS family"),
            };
            let source = data[2];
            ensure!(source <= bits && data[3] <= bits, "invalid ECS prefix");
            ensure!(
                data.len() == 4 + usize::from(source.div_ceil(8)),
                "invalid ECS address length"
            );
            if !source.is_multiple_of(8) {
                ensure!(
                    data[data.len() - 1] & ((1u8 << (8 - source % 8)) - 1) == 0,
                    "nonzero ECS padding bits"
                );
            }
        }
    }
    Ok(())
}
