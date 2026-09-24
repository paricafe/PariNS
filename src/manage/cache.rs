//! Management input validation only; cache semantics stay in crate::cache.
use crate::ecs::Scope;
use anyhow::{Result, ensure};
use hickory_proto::{
    op::{Edns, Message, MessageType, OpCode, Query},
    rr::{Name, RecordType, rdata::opt::ClientSubnet},
};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct Inspect {
    pub name: String,
    pub qtype: String,
    pub subnet: Option<String>,
    pub edns: bool,
    pub dnssec_ok: bool,
    pub checking_disabled: bool,
    pub recursion_desired: bool,
}
impl Default for Inspect {
    fn default() -> Self {
        Self {
            name: String::new(),
            qtype: "A".into(),
            subnet: None,
            edns: false,
            dnssec_ok: false,
            checking_disabled: false,
            recursion_desired: true,
        }
    }
}

pub(super) fn name(value: &str) -> Result<Name> {
    ensure!(
        !value.is_empty() && value.len() <= 254,
        "name must be a DNS name (1..254 bytes)"
    );
    let mut name = Name::from_ascii(value)?;
    name.set_fqdn(true);
    ensure!(!name.is_root(), "root inspection is not supported");
    Ok(name.to_lowercase())
}

impl Inspect {
    pub fn prepare(&self) -> Result<(Message, Option<ClientSubnet>)> {
        let name = name(&self.name)?;
        let kind: RecordType = self.qtype.to_ascii_uppercase().parse()?;
        let subnet = self
            .subnet
            .as_ref()
            .map(|text| {
                let net: ipnet::IpNet = text.parse()?;
                Ok::<_, anyhow::Error>(ClientSubnet::new(net.trunc().addr(), net.prefix_len(), 0))
            })
            .transpose()?;
        let mut query = Message::new(0, MessageType::Query, OpCode::Query);
        query.add_query(Query::query(name, kind));
        query.metadata.recursion_desired = self.recursion_desired;
        query.metadata.checking_disabled = self.checking_disabled;
        ensure!(self.edns || !self.dnssec_ok, "DO requires EDNS");
        if self.edns {
            let mut edns = Edns::new();
            edns.set_dnssec_ok(self.dnssec_ok);
            query.edns = Some(edns);
        }
        Ok((query, subnet))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Invalidate {
    pub revision: u64,
    pub epoch: u64,
    #[serde(default)]
    pub all: bool,
    pub name: Option<String>,
    pub qtype: Option<String>,
    pub scope: Option<String>,
}

pub(super) type Selection = (Option<String>, Option<RecordType>, Option<Scope>);
impl Invalidate {
    pub fn selection(&self) -> Result<Selection> {
        ensure!(
            self.all != self.name.is_some(),
            "choose all=true or a specific name"
        );
        ensure!(
            !self.all || (self.qtype.is_none() && self.scope.is_none()),
            "all cannot be combined with filters"
        );
        let name = self
            .name
            .as_ref()
            .map(|value| name(value).map(|name| name.to_ascii()))
            .transpose()?;
        let kind = self
            .qtype
            .as_ref()
            .map(|value| value.to_ascii_uppercase().parse())
            .transpose()?;
        let scope = self.scope.as_deref().map(Scope::parse_tag).transpose()?;
        Ok((name, kind, scope))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn inspection_normalizes_without_changing_edns_namespace() {
        let inspect: Inspect =
            serde_json::from_value(json!({"name":"EXAMPLE.test", "subnet":"192.0.2.7/24"}))
                .unwrap();
        let (query, subnet) = inspect.prepare().unwrap();
        assert_eq!(query.queries[0].name().to_ascii(), "example.test.");
        assert!(query.edns.is_none());
        assert_eq!(subnet.unwrap().addr().to_string(), "192.0.2.0");
        assert!(Inspect::default().prepare().is_err());
    }
    #[test]
    fn invalidation_requires_explicit_scope_and_rejects_ambiguous_clear() {
        for value in [
            json!({"revision":1,"epoch":0}),
            json!({"revision":1,"epoch":0,"all":true,"name":"test"}),
            json!({"revision":1,"epoch":0,"all":true,"qtype":"A"}),
        ] {
            assert!(
                serde_json::from_value::<Invalidate>(value)
                    .unwrap()
                    .selection()
                    .is_err()
            );
        }
        let input: Invalidate = serde_json::from_value(
            json!({"revision":1,"epoch":0,"name":"TEST","scope":"privacy_v6"}),
        )
        .unwrap();
        assert_eq!(
            input.selection().unwrap(),
            (
                Some("test.".into()),
                None,
                Some(Scope::Privacy { ipv4: false })
            )
        );
    }
}
