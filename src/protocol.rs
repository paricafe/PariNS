//! DNS semantics only; no sockets, scheduling, or configuration loading.

use anyhow::{Result, ensure};
use hickory_proto::{
    op::{Edns, Header, Message, MessageType, OpCode, ResponseCode},
    rr::RecordType,
    serialize::binary::{BinDecodable, BinDecoder},
};

pub const MAX_MESSAGE: usize = u16::MAX as usize;
pub const MAX_UDP_PAYLOAD: u16 = 1232;

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
}

pub fn udp_limit(query: &Message) -> usize {
    query
        .edns
        .as_ref()
        .map_or(512, |edns| edns.max_payload().clamp(512, MAX_UDP_PAYLOAD)) as usize
}

pub fn encode_udp(response: &Message, limit: usize) -> Result<Vec<u8>> {
    let bytes = response.to_vec()?;
    if bytes.len() <= limit {
        return Ok(bytes);
    }
    // Return a complete question-only TC response rather than a partial RRset.
    let mut truncated = error_response(response, response.response_code);
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
