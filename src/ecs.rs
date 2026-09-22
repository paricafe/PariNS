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
    Privacy { ipv4: bool },
    Network(IpNet),
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

    pub fn cache_scope(&self, response: &Message) -> Option<Scope> {
        match (self.outgoing, subnet(response)) {
            (None, None) => Some(Scope::NoEcs),
            (Some(sent), Some(received)) if received.scope_prefix() <= sent.source_prefix() => {
                if sent.source_prefix() == 0 {
                    Some(Scope::Privacy {
                        ipv4: sent.addr().is_ipv4(),
                    })
                } else {
                    Some(Scope::Network(
                        IpNet::new(sent.addr(), received.scope_prefix())
                            .ok()?
                            .trunc(),
                    ))
                }
            }
            _ => None,
        }
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
            _ => false,
        }
    }

    pub fn prefix_len(self) -> u8 {
        match self {
            Self::Network(network) => network.prefix_len(),
            _ => 0,
        }
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
