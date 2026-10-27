//! What a blocked query is answered with: the configured method, its rule-line
//! syntax, and the synthesized answer (addresses or SOA, TTL, EDE 15).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use anyhow::{Result, bail, ensure};
use hickory_proto::{
    op::{Message, ResponseCode},
    rr::{
        DNSClass, Name, RData, Record, RecordType,
        rdata::{A, AAAA, SOA, opt::EdnsOption},
    },
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// RFC 8914 Extended DNS Error option code and the "Blocked" info code.
const EDE: u16 = 15;
const EDE_BLOCKED: u16 = 15;
pub(super) const DEFAULT_TTL_SECS: u32 = 10;
pub(super) const MAX_TTL_SECS: u32 = 86_400;

/// The configured method names, as written in `[filter]` and subscription sources.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BlockMode {
    #[default]
    Nodata,
    NullIp,
    CustomIp,
    Nxdomain,
    Refused,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BlockResponse {
    /// NOERROR without records, with a synthesized SOA.
    Nodata,
    /// A answers 0.0.0.0 and AAAA answers ::.
    NullIp,
    /// A and AAAA answer the configured address of their family, if any.
    CustomIp {
        ipv4: Option<Ipv4Addr>,
        ipv6: Option<Ipv6Addr>,
    },
    /// NXDOMAIN with a synthesized SOA.
    Nxdomain,
    Refused,
}

impl BlockResponse {
    /// Global `[filter]` and subscription-source settings: addresses belong to
    /// `custom_ip` only, which needs at least one.
    pub fn from_settings(
        mode: BlockMode,
        ipv4: Option<Ipv4Addr>,
        ipv6: Option<Ipv6Addr>,
    ) -> Result<Self> {
        if mode != BlockMode::CustomIp {
            ensure!(
                ipv4.is_none() && ipv6.is_none(),
                "block_ipv4 and block_ipv6 are allowed only with block_response = \"custom_ip\""
            );
        }
        Ok(match mode {
            BlockMode::Nodata => Self::Nodata,
            BlockMode::NullIp => Self::NullIp,
            BlockMode::Nxdomain => Self::Nxdomain,
            BlockMode::Refused => Self::Refused,
            BlockMode::CustomIp => {
                ensure!(
                    ipv4.is_some() || ipv6.is_some(),
                    "block_response = \"custom_ip\" needs block_ipv4, block_ipv6 or both"
                );
                for address in [ipv4.map(IpAddr::V4), ipv6.map(IpAddr::V6)]
                    .into_iter()
                    .flatten()
                {
                    ensure!(
                        unicast(address),
                        "block_ipv4 and block_ipv6 must be unicast addresses"
                    );
                }
                Self::CustomIp { ipv4, ipv6 }
            }
        })
    }

    /// The method written after a block rule's domain: a method name, or one or
    /// two unicast addresses (at most one per family) for a custom IP.
    pub(super) fn parse_rule(tokens: &[&str]) -> Result<Self> {
        match tokens {
            ["nodata"] => return Ok(Self::Nodata),
            ["null_ip"] => return Ok(Self::NullIp),
            ["nxdomain"] => return Ok(Self::Nxdomain),
            ["refused"] => return Ok(Self::Refused),
            [_] | [_, _] => {}
            _ => bail!("expected one method or one or two IP addresses after the domain"),
        }
        let (mut ipv4, mut ipv6) = (None, None);
        for token in tokens {
            let Ok(address) = token.parse::<IpAddr>() else {
                bail!("unknown method: use nodata, null_ip, nxdomain, refused or IP addresses");
            };
            ensure!(unicast(address), "the address is not a unicast address");
            let duplicate = match address {
                IpAddr::V4(v4) => ipv4.replace(v4).is_some(),
                IpAddr::V6(v6) => ipv6.replace(v6).is_some(),
            };
            ensure!(!duplicate, "at most one IPv4 and one IPv6 address");
        }
        Ok(Self::CustomIp { ipv4, ipv6 })
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Nodata => "nodata",
            Self::NullIp => "null_ip",
            Self::CustomIp { .. } => "custom_ip",
            Self::Nxdomain => "nxdomain",
            Self::Refused => "refused",
        }
    }

    pub(super) fn digest(&self, hash: &mut Sha256) {
        hash.update(self.label().as_bytes());
        hash.update([0]);
    }
    /// Names the method and its addresses.
    pub(super) fn label(&self) -> String {
        match self {
            Self::CustomIp { ipv4, ipv6 } => format!("custom_ip {ipv4:?} {ipv6:?}"),
            other => other.kind().into(),
        }
    }

    /// The answer to `query` blocked by `rule`. Only IN-class A and AAAA
    /// questions get addresses; every other question is answered as nodata
    /// (or NXDOMAIN or REFUSED). It never carries AD.
    pub(super) fn answer(&self, query: &Message, rule: &Name, ttl: u32) -> Message {
        let question = &query.queries[0];
        let code = match self {
            Self::Nxdomain => ResponseCode::NXDomain,
            Self::Refused => ResponseCode::Refused,
            _ => ResponseCode::NoError,
        };
        let mut response = crate::protocol::error_response(query, code);
        let address = match (self, question.query_class(), question.query_type()) {
            (Self::NullIp, DNSClass::IN, RecordType::A) => Some(RData::A(A(Ipv4Addr::UNSPECIFIED))),
            (Self::NullIp, DNSClass::IN, RecordType::AAAA) => {
                Some(RData::AAAA(AAAA(Ipv6Addr::UNSPECIFIED)))
            }
            (
                Self::CustomIp {
                    ipv4: Some(address),
                    ..
                },
                DNSClass::IN,
                RecordType::A,
            ) => Some(RData::A(A(*address))),
            (
                Self::CustomIp {
                    ipv6: Some(address),
                    ..
                },
                DNSClass::IN,
                RecordType::AAAA,
            ) => Some(RData::AAAA(AAAA(*address))),
            _ => None,
        };
        match address {
            Some(data) => {
                let mut record = Record::from_rdata(question.name().clone(), ttl, data);
                record.dns_class = question.query_class();
                response.add_answer(record);
            }
            None if code != ResponseCode::Refused => {
                response.add_authority(soa(rule, question.query_class(), ttl));
            }
            None => {}
        }
        if let Some(edns) = response.edns.as_mut() {
            edns.options_mut()
                .insert(EdnsOption::Unknown(EDE, EDE_BLOCKED.to_be_bytes().to_vec()));
        }
        response.metadata.authentic_data = false;
        response
    }
}

/// Owned by the blocking rule; MINIMUM is the TTL so clients cache the
/// negative answer exactly as long (RFC 2308).
fn soa(rule: &Name, class: DNSClass, ttl: u32) -> Record {
    let mut record = Record::from_rdata(
        rule.clone(),
        ttl,
        RData::SOA(SOA::new(
            Name::from_ascii("blocked.parins.invalid.").expect("static name"),
            Name::from_ascii("hostmaster.parins.invalid.").expect("static name"),
            1,
            1800,
            900,
            604_800,
            ttl,
        )),
    );
    record.dns_class = class;
    record
}

fn unicast(address: IpAddr) -> bool {
    !address.is_unspecified()
        && !address.is_multicast()
        && address != IpAddr::V4(Ipv4Addr::BROADCAST)
}

/// The block method for every index slot. Slots below `local_slots` are local
/// rules (slot 0 follows the global method, the rest are distinct per-rule
/// methods in first-declaration order); the others are subscription sources.
#[derive(Clone, Debug)]
pub(crate) struct Responses {
    pub global: BlockResponse,
    pub ttl: u32,
    pub local_slots: u16,
    pub slots: Vec<Option<BlockResponse>>,
}

impl Responses {
    pub fn get(&self, slot: u16) -> BlockResponse {
        self.slots
            .get(usize::from(slot))
            .copied()
            .flatten()
            .unwrap_or(self.global)
    }
    /// The answer class of every slot: slots with one class answer alike.
    pub fn classes(&self) -> Vec<u16> {
        let mut distinct: Vec<BlockResponse> = Vec::new();
        (0..self.slots.len() as u16)
            .map(|slot| {
                let response = self.get(slot);
                distinct
                    .iter()
                    .position(|known| *known == response)
                    .unwrap_or_else(|| {
                        distinct.push(response);
                        distinct.len() - 1
                    }) as u16
            })
            .collect()
    }
    pub fn digest(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(b"parins-block-responses-v1\0");
        self.global.digest(&mut hash);
        hash.update(self.ttl.to_be_bytes());
        hash.update(self.local_slots.to_be_bytes());
        for slot in &self.slots {
            match slot {
                None => hash.update([0]),
                Some(response) => {
                    hash.update([1]);
                    response.digest(&mut hash);
                }
            }
        }
        hash.finalize().into()
    }
}
