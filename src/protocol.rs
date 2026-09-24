//! DNS semantics only; no sockets, scheduling, or configuration loading.

use anyhow::{Result, ensure};
use hickory_proto::{
    op::{Edns, Header, Message, MessageType, OpCode, ResponseCode},
    rr::{
        DNSClass, RecordType,
        rdata::opt::{EdnsCode, EdnsOption},
    },
    serialize::binary::{BinDecodable, BinDecoder},
};

pub const MAX_MESSAGE: usize = u16::MAX as usize;
pub const MAX_UDP_PAYLOAD: u16 = 1232;

pub fn has_padding(message: &Message) -> bool {
    message
        .edns
        .as_ref()
        .is_some_and(|e| e.option(EdnsCode::from(12)).is_some())
}

pub fn strip_padding(message: &mut Message) {
    if let Some(edns) = &mut message.edns {
        edns.options_mut().remove(EdnsCode::from(12));
    }
}

/// Per-hop final encoding, after ID, ECS, policy and framing decisions.
/// The unpadded hot path encodes exactly once. Padding never truncates answers.
pub fn encode_hop(
    message: &Message,
    encrypted: bool,
    requested: bool,
    block: usize,
    limit: usize,
) -> Result<Vec<u8>> {
    let pad = encrypted && requested && message.edns.is_some();
    if !pad && !has_padding(message) {
        return Ok(message.to_vec()?);
    }
    let mut normalized = message.clone();
    strip_padding(&mut normalized);
    let bytes = normalized.to_vec()?;
    let limit = limit.min(MAX_MESSAGE);
    if !pad || bytes.len().saturating_add(4) > limit {
        return Ok(bytes);
    }
    ensure!(block > 0, "padding block must be nonzero");
    let with_header = bytes.len() + 4;
    let length = ((block - with_header % block) % block).min(limit - with_header);
    normalized
        .edns
        .as_mut()
        .expect("EDNS checked")
        .options_mut()
        .insert(EdnsOption::Unknown(12, vec![0; length]));
    Ok(normalized.to_vec()?)
}

#[derive(Clone, Copy, Default)]
pub struct Padding {
    pub requested: bool,
    pub limit: usize,
}

impl Padding {
    pub fn from_query(query: &Message) -> Self {
        Self {
            requested: has_padding(query),
            limit: query
                .edns
                .as_ref()
                .map_or(512, |e| usize::from(e.max_payload()).max(512)),
        }
    }
    pub fn encode_response(self, message: &Message, encrypted: bool) -> Result<Vec<u8>> {
        encode_hop(message, encrypted, self.requested, 468, self.limit)
    }
}

pub fn encode_upstream(message: &Message, encrypted: bool) -> Result<Vec<u8>> {
    let intent = Padding::from_query(message);
    encode_hop(message, encrypted, intent.requested, 128, intent.limit)
}

/// Flight and refresh share one semantic key. Padding intent remains one bit;
/// its arbitrary payload cannot create unbounded independent work groups.
pub fn canonical_work_key(query: &Message) -> Option<Vec<u8>> {
    if query.queries.len() != 1
        || query.queries[0].query_class() != DNSClass::IN
        || query.signature.is_some()
        || query.edns.as_ref().is_some_and(|e| {
            e.options()
                .options
                .iter()
                .any(|(code, _)| !matches!(u16::from(*code), 8 | 12))
        })
    {
        return None;
    }
    let mut normalized = query.clone();
    normalized.metadata.id = 0;
    normalized.queries[0].set_name(query.queries[0].name().to_lowercase());
    if has_padding(&normalized) {
        strip_padding(&mut normalized);
        normalized
            .edns
            .as_mut()?
            .options_mut()
            .insert(EdnsOption::Unknown(12, vec![]));
    }
    normalized.to_vec().ok()
}

pub enum Request {
    Forward(Message),
    Reply(Message),
    Drop,
}

pub fn decode(bytes: &[u8]) -> Result<Message> {
    ensure!(bytes.len() <= MAX_MESSAGE, "DNS message too large");
    let mut decoder = BinDecoder::new(bytes);
    let message = Message::read(&mut decoder)?;
    ensure!(decoder.is_empty(), "trailing bytes after DNS message");
    // Malformed OPT tails can make the library discard all typed options,
    // including an ECS /0 privacy request. Validate the original envelope.
    if message.edns.is_some() {
        crate::ecs::validate_wire(bytes)?;
    }
    Ok(message)
}

pub fn request(bytes: &[u8]) -> Request {
    let query = match decode(bytes) {
        Ok(query) => query,
        Err(_) => {
            let Ok(header) = Header::read(&mut BinDecoder::new(bytes)) else {
                return Request::Drop;
            };
            if header.message_type == MessageType::Response {
                return Request::Drop;
            }
            return Request::Reply(Message::error_msg(
                header.id,
                header.op_code,
                ResponseCode::FormErr,
            ));
        }
    };
    if query.message_type != MessageType::Query {
        return Request::Drop;
    }
    let error = if query.op_code != OpCode::Query {
        Some(ResponseCode::NotImp)
    } else if query.queries.len() != 1
        || query.truncation
        || query.response_code != ResponseCode::NoError
    {
        Some(ResponseCode::FormErr)
    } else if query.signature.is_some()
        || query
            .additionals
            .iter()
            .any(|rr| rr.record_type() == RecordType::SIG)
        || matches!(
            query.queries[0].query_type(),
            RecordType::AXFR | RecordType::IXFR | RecordType::ANY
        )
    {
        Some(ResponseCode::Refused)
    } else if !query.answers.is_empty()
        || !query.authorities.is_empty()
        || !query.additionals.is_empty()
    {
        Some(ResponseCode::FormErr)
    } else if query.edns.as_ref().is_some_and(|edns| edns.version() != 0) {
        Some(ResponseCode::BADVERS)
    } else {
        None
    };
    match error {
        Some(code) => Request::Reply(error_response(&query, code)),
        None => Request::Forward(query),
    }
}

pub fn error_response(query: &Message, code: ResponseCode) -> Message {
    let mut response = Message::error_msg(query.id, query.op_code, code);
    // Invalid multi-question messages do not need to be echoed in full.
    response
        .queries
        .extend(query.queries.iter().take(1).cloned());
    response.metadata.recursion_desired = query.recursion_desired;
    response.metadata.checking_disabled = query.checking_disabled;
    response.metadata.recursion_available = true;
    if let Some(edns) = &query.edns {
        let mut reply_edns = Edns::new();
        reply_edns
            .set_max_payload(MAX_UDP_PAYLOAD)
            .set_dnssec_ok(edns.flags().dnssec_ok);
        response.edns = Some(reply_edns);
    }
    response
}

pub fn matches_response(query: &Message, response: &Message) -> bool {
    response.message_type == MessageType::Response
        && response.id == query.id
        && response.op_code == query.op_code
        && response.queries == query.queries
        && response.signature.is_none()
        && crate::ecs::response_matches(query, response)
}

pub fn udp_limit(query: &Message) -> usize {
    query
        .edns
        .as_ref()
        .map_or(512, |edns| edns.max_payload().clamp(512, MAX_UDP_PAYLOAD)) as usize
}

pub fn encode_udp(response: &Message, limit: usize) -> Result<Vec<u8>> {
    let bytes = encode_hop(response, false, false, 468, limit)?;
    if bytes.len() <= limit {
        return Ok(bytes);
    }
    // Return a complete question-only TC response rather than a partial RRset.
    let mut truncated = error_response(response, response.response_code);
    crate::ecs::set_subnet(&mut truncated, crate::ecs::subnet(response));
    truncated.metadata.truncation = true;
    let bytes = truncated.to_vec()?;
    ensure!(
        bytes.len() <= limit,
        "truncated response exceeds UDP budget"
    );
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hickory_proto::{op::Query, rr::Name};

    pub fn query() -> Message {
        let mut query = Message::new(7, MessageType::Query, OpCode::Query);
        query.add_query(Query::query(
            Name::from_ascii("example.test.").unwrap(),
            RecordType::A,
        ));
        query
    }

    #[test]
    fn padding_is_per_hop_bounded_and_never_removes_answers() {
        let mut q = query();
        let mut edns = Edns::new();
        edns.set_max_payload(1232);
        edns.options_mut()
            .insert(EdnsOption::Unknown(12, vec![77; 19]));
        edns.options_mut()
            .insert(EdnsOption::Unknown(10, vec![3; 8]));
        q.edns = Some(edns);
        let mut r = error_response(&q, ResponseCode::NoError);
        r.edns = q.edns.clone();
        let padded = Padding::from_query(&q).encode_response(&r, true).unwrap();
        assert_eq!(padded.len() % 468, 0);
        let decoded = decode(&padded).unwrap();
        assert_eq!(decoded.id, q.id);
        assert_eq!(
            decoded.edns.as_ref().unwrap().option(EdnsCode::from(10)),
            q.edns.as_ref().unwrap().option(EdnsCode::from(10))
        );
        let plain = Padding::from_query(&q).encode_response(&r, false).unwrap();
        assert!(!has_padding(&decode(&plain).unwrap()));
        let mut base = r.clone();
        strip_padding(&mut base);
        let base_len = base.to_vec().unwrap().len();
        for spare in 0..10 {
            let bytes = encode_hop(&r, true, true, 468, base_len + spare).unwrap();
            assert!(bytes.len() <= base_len + spare);
            let answer = decode(&bytes).unwrap();
            assert_eq!(answer.answers, r.answers);
            assert!(!answer.truncation);
            assert_eq!(has_padding(&answer), spare >= 4);
        }
        assert_eq!(
            encode_hop(&q, true, true, 128, MAX_MESSAGE).unwrap().len() % 128,
            0
        );
        assert!(!has_padding(
            &decode(&encode_hop(&r, true, false, 468, MAX_MESSAGE).unwrap()).unwrap()
        ));
        let bare = query();
        assert!(
            decode(&encode_hop(&bare, true, true, 468, MAX_MESSAGE).unwrap())
                .unwrap()
                .edns
                .is_none()
        );
    }

    #[test]
    fn work_key_ignores_padding_payload_but_preserves_intent_and_edns_boundary() {
        let mut a = query();
        a.edns = Some(Edns::new());
        let no_padding = canonical_work_key(&a).unwrap();
        a.edns
            .as_mut()
            .unwrap()
            .options_mut()
            .insert(EdnsOption::Unknown(12, vec![]));
        let padded = canonical_work_key(&a).unwrap();
        assert_ne!(no_padding, padded);
        a.edns
            .as_mut()
            .unwrap()
            .options_mut()
            .insert(EdnsOption::Unknown(12, vec![42; 201]));
        assert_eq!(canonical_work_key(&a).unwrap(), padded);
        for code in [10, 65001] {
            let mut other = a.clone();
            other
                .edns
                .as_mut()
                .unwrap()
                .options_mut()
                .insert(EdnsOption::Unknown(code, vec![0; 8]));
            assert!(canonical_work_key(&other).is_none());
        }
    }

    #[test]
    fn validates_queries_and_rejects_trailing_bytes() {
        let mut q = query();
        assert!(matches!(request(&q.to_vec().unwrap()), Request::Forward(_)));
        q.metadata.op_code = OpCode::Update;
        assert!(
            matches!(request(&q.to_vec().unwrap()), Request::Reply(r) if r.response_code == ResponseCode::NotImp)
        );
        q.metadata.op_code = OpCode::Query;
        q.queries.push(q.queries[0].clone());
        assert!(
            matches!(request(&q.to_vec().unwrap()), Request::Reply(r) if r.response_code == ResponseCode::FormErr)
        );
        let mut bytes = query().to_vec().unwrap();
        bytes.push(0);
        assert!(
            matches!(request(&bytes), Request::Reply(r) if r.response_code == ResponseCode::FormErr)
        );
        assert!(matches!(request(&[0, 1]), Request::Drop));
    }

    #[test]
    fn response_correlation_checks_identity_and_question() {
        let q = query();
        let mut r = error_response(&q, ResponseCode::NoError);
        assert!(matches_response(&q, &r));
        r.metadata.id += 1;
        assert!(!matches_response(&q, &r));
        r.metadata.id = q.id;
        r.queries[0].set_query_type(RecordType::AAAA);
        assert!(!matches_response(&q, &r));
    }

    #[test]
    fn rejects_transfers_and_negotiates_edns_version() {
        let mut q = query();
        q.queries[0].set_query_type(RecordType::AXFR);
        assert!(
            matches!(request(&q.to_vec().unwrap()), Request::Reply(r) if r.response_code == ResponseCode::Refused)
        );
        q.queries[0].set_query_type(RecordType::A);
        let mut edns = Edns::new();
        edns.set_version(1);
        q.edns = Some(edns);
        let Request::Reply(r) = request(&q.to_vec().unwrap()) else {
            panic!("expected BADVERS")
        };
        let decoded = decode(&r.to_vec().unwrap()).unwrap();
        // BADVERS and TSIG BADSIG share wire code 16; this is an unsigned EDNS response.
        assert_eq!(u16::from(decoded.response_code), 16);
        assert_eq!(decoded.edns.unwrap().version(), 0);
    }

    #[test]
    fn edns_payload_is_bounded_and_oversize_answers_remain_decodable() {
        use hickory_proto::rr::{RData, Record, rdata::A};
        let mut q = query();
        assert_eq!(udp_limit(&q), 512);
        let mut edns = Edns::new();
        edns.set_max_payload(4096).set_dnssec_ok(true);
        q.edns = Some(edns);
        q.metadata.checking_disabled = true;
        assert_eq!(udp_limit(&q), 1232);
        let mut response = error_response(&q, ResponseCode::NoError);
        for _ in 0..100 {
            response.add_answer(Record::from_rdata(
                q.queries[0].name().clone(),
                60,
                RData::A(A::new(192, 0, 2, 1)),
            ));
        }
        let bytes = encode_udp(&response, udp_limit(&q)).unwrap();
        let truncated = decode(&bytes).unwrap();
        assert!(bytes.len() <= 1232);
        assert!(truncated.truncation);
        assert_eq!(truncated.queries, q.queries);
        assert!(truncated.checking_disabled);
        assert!(truncated.edns.unwrap().flags().dnssec_ok);
    }
}
