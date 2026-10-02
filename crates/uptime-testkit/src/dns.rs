//! A tiny DNS server for tests: answers A, AAAA, CNAME and TXT queries from a
//! fixed zone, following CNAME chains like a recursive resolver would.

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
};

use hickory_resolver::proto::{
    op::{Message, MessageType, ResponseCode},
    rr::{
        Name, RData, Record, RecordType,
        rdata::{A, AAAA, CNAME, TXT},
    },
};
use tokio::net::UdpSocket;

/// A tiny recursive-looking DNS server answering from a fixed zone.
#[derive(Default)]
pub struct Zone {
    cnames: HashMap<String, String>,
    addresses: HashMap<String, Vec<IpAddr>>,
    texts: HashMap<String, Vec<String>>,
}

impl Zone {
    pub fn cname(mut self, from: &str, to: &str) -> Self {
        self.cnames.insert(from.into(), to.into());
        self
    }

    pub fn address(mut self, name: &str, ip: &str) -> Self {
        self.addresses
            .entry(name.into())
            .or_default()
            .push(ip.parse().expect("test DNS server"));
        self
    }

    pub fn txt(mut self, name: &str, text: &str) -> Self {
        self.texts.entry(name.into()).or_default().push(text.into());
        self
    }

    fn exists(&self, name: &str) -> bool {
        self.cnames.contains_key(name)
            || self.addresses.contains_key(name)
            || self.texts.contains_key(name)
    }

    fn answer(&self, query: &Message) -> Message {
        let mut response = Message::response(query.metadata.id, query.metadata.op_code);
        response.metadata.recursion_desired = query.metadata.recursion_desired;
        response.metadata.recursion_available = true;
        response.queries.clone_from(&query.queries);
        let Some(question) = query.queries.first() else {
            return response;
        };
        let mut name = question.name().to_ascii().trim_end_matches('.').to_owned();
        if !self.exists(&name) {
            response.metadata.response_code = ResponseCode::NXDomain;
            return response;
        }
        let record = |owner: &str, data: RData| {
            Record::from_rdata(
                Name::from_ascii(format!("{owner}.")).expect("test DNS server"),
                60,
                data,
            )
        };
        let wanted = question.query_type();
        let mut hops = 0;
        while let Some(target) = self.cnames.get(&name) {
            hops += 1;
            if hops > 16 {
                break;
            }
            let data = RData::CNAME(CNAME(
                Name::from_ascii(format!("{target}.")).expect("test DNS server"),
            ));
            response.answers.push(record(&name, data));
            if wanted == RecordType::CNAME {
                return response;
            }
            name.clone_from(target);
        }
        for ip in self.addresses.get(&name).into_iter().flatten() {
            match (ip, wanted) {
                (IpAddr::V4(v4), RecordType::A) => {
                    response.answers.push(record(&name, RData::A(A(*v4))));
                }
                (IpAddr::V6(v6), RecordType::AAAA) => {
                    response.answers.push(record(&name, RData::AAAA(AAAA(*v6))));
                }
                _ => {}
            }
        }
        if wanted == RecordType::TXT {
            for text in self.texts.get(&name).into_iter().flatten() {
                let data = RData::TXT(TXT::new(vec![text.clone()]));
                response.answers.push(record(&name, data));
            }
        }
        response
    }

    /// Serves the zone over UDP on localhost until the runtime shuts down.
    pub async fn serve(self) -> SocketAddr {
        let socket = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("test DNS server");
        let addr = socket.local_addr().expect("test DNS server");
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            while let Ok((len, peer)) = socket.recv_from(&mut buf).await {
                let Ok(query) = Message::from_vec(&buf[..len]) else {
                    continue;
                };
                if query.metadata.message_type != MessageType::Query {
                    continue;
                }
                let bytes = self.answer(&query).to_vec().expect("test DNS server");
                let _ = socket.send_to(&bytes, peer).await;
            }
        });
        addr
    }
}
