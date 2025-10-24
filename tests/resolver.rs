use std::{
    net::{Ipv4Addr, Ipv6Addr},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use codecrafters_dns_server::{
    resolver::DnsResolver, BytePacketBuf, Dns, DnsRecord, QueryType, ResultCode,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, UdpSocket},
    sync::Notify,
};

fn copy_into_packet(bytes: &[u8]) -> Dns {
    let mut packet = BytePacketBuf::new();
    packet.as_mut_slice()[..bytes.len()].copy_from_slice(bytes);
    packet.set_len(bytes.len()).unwrap();
    packet.seek(0).unwrap();
    Dns::parse_req(&mut packet).unwrap()
}

fn make_response(id: u16, qname: &str, record: DnsRecord) -> Vec<u8> {
    let mut buffer = BytePacketBuf::new();
    buffer.write_u16(id).unwrap();
    buffer.write_u16(0x8180).unwrap();
    buffer.write_u16(1).unwrap();
    buffer.write_u16(1).unwrap();
    buffer.write_u16(0).unwrap();
    buffer.write_u16(0).unwrap();
    buffer.write_qname(qname).unwrap();
    let qtype = match record {
        DnsRecord::A { .. } => QueryType::A,
        DnsRecord::AAAA { .. } => QueryType::AAAA,
        DnsRecord::MX { .. } => QueryType::MX,
        DnsRecord::NS { .. } => QueryType::NS,
        DnsRecord::TXT { .. } => QueryType::TXT,
        _ => panic!("unsupported record in test"),
    };
    buffer.write_u16(u16::from(qtype)).unwrap();
    buffer.write_u16(1).unwrap();
    record.write(&mut buffer).unwrap();
    let len = buffer.pos();
    buffer.set_len(len).unwrap();
    buffer.as_slice().to_vec()
}

#[tokio::test]
async fn resolver_negotiates_edns_and_aggregates_records() {
    let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = udp.local_addr().unwrap();
    let handled = Arc::new(AtomicUsize::new(0));
    let udp_task = {
        let handled = handled.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            loop {
                let (len, peer) = udp.recv_from(&mut buf).await.unwrap();
                let message = copy_into_packet(&buf[..len]);
                assert_eq!(message.header.qdcount, 1);
                assert_eq!(message.header.arcount, 1);
                let opt = message
                    .additional
                    .iter()
                    .find_map(|record| match record {
                        DnsRecord::OPT { payload_size, .. } => Some(*payload_size),
                        _ => None,
                    })
                    .unwrap();
                assert_eq!(opt, 1232);
                assert_eq!(message.header.flags.rescode, ResultCode::NOERROR);

                let question = message.question.first().unwrap();
                let ttl = match question.qtype {
                    QueryType::A => 120,
                    QueryType::AAAA => 240,
                    QueryType::MX => 360,
                    QueryType::NS => 480,
                    QueryType::TXT => 600,
                    _ => 0,
                };
                let response = match question.qtype {
                    QueryType::A => make_response(
                        message.header.id,
                        &question.qname,
                        DnsRecord::A {
                            domain: question.qname.clone(),
                            addr: Ipv4Addr::new(1, 2, 3, 4),
                            ttl,
                        },
                    ),
                    QueryType::AAAA => make_response(
                        message.header.id,
                        &question.qname,
                        DnsRecord::AAAA {
                            domain: question.qname.clone(),
                            addr: Ipv6Addr::LOCALHOST,
                            ttl,
                        },
                    ),
                    QueryType::MX => make_response(
                        message.header.id,
                        &question.qname,
                        DnsRecord::MX {
                            domain: question.qname.clone(),
                            preference: 10,
                            exchange: "mail.example.com".to_string(),
                            ttl,
                        },
                    ),
                    QueryType::NS => make_response(
                        message.header.id,
                        &question.qname,
                        DnsRecord::NS {
                            domain: question.qname.clone(),
                            host: "ns1.example.com".to_string(),
                            ttl,
                        },
                    ),
                    QueryType::TXT => make_response(
                        message.header.id,
                        &question.qname,
                        DnsRecord::TXT {
                            domain: question.qname.clone(),
                            data: vec!["hello".to_string()],
                            ttl,
                        },
                    ),
                    _ => unreachable!(),
                };
                udp.send_to(&response, peer).await.unwrap();
                if handled.fetch_add(1, Ordering::SeqCst) + 1 == 5 {
                    break;
                }
            }
        })
    };

    let resolver = DnsResolver::new(addr, Duration::from_millis(500));
    let resolution = resolver
        .resolve("example.com")
        .await
        .expect("resolution succeeded");

    assert_eq!(resolution.a_records.len(), 1);
    assert_eq!(resolution.a_records[0].addr, Ipv4Addr::new(1, 2, 3, 4));
    assert_eq!(resolution.aaaa_records.len(), 1);
    assert_eq!(resolution.aaaa_records[0].addr, Ipv6Addr::LOCALHOST);
    assert_eq!(resolution.mx_records.len(), 1);
    assert_eq!(resolution.mx_records[0].exchange, "mail.example.com");
    assert_eq!(resolution.ns_records.len(), 1);
    assert_eq!(resolution.ns_records[0].host, "ns1.example.com");
    assert_eq!(resolution.txt_records.len(), 1);
    assert_eq!(resolution.txt_records[0].data[0], "hello");
    assert_eq!(resolution.ttl, Duration::from_secs(120));

    udp_task.await.unwrap();
}

#[tokio::test]
async fn resolver_falls_back_to_tcp_on_truncation() {
    let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = udp.local_addr().unwrap();
    let tcp = TcpListener::bind(addr).await.unwrap();
    let udp_requests = Arc::new(AtomicUsize::new(0));
    let tcp_requests = Arc::new(AtomicUsize::new(0));
    let udp_task = {
        let udp_requests = udp_requests.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            let (len, peer) = udp.recv_from(&mut buf).await.unwrap();
            udp_requests.fetch_add(1, Ordering::SeqCst);
            let message = copy_into_packet(&buf[..len]);
            let mut buffer = BytePacketBuf::new();
            buffer.write_u16(message.header.id).unwrap();
            buffer.write_u16(0x8380).unwrap();
            buffer.write_u16(1).unwrap();
            buffer.write_u16(0).unwrap();
            buffer.write_u16(0).unwrap();
            buffer.write_u16(0).unwrap();
            let question = message.question.first().unwrap();
            buffer.write_qname(&question.qname).unwrap();
            buffer.write_u16(u16::from(question.qtype.clone())).unwrap();
            buffer.write_u16(1).unwrap();
            let len = buffer.pos();
            buffer.set_len(len).unwrap();
            udp.send_to(buffer.as_slice(), peer).await.unwrap();
        })
    };
    let tcp_task = {
        let tcp_requests = tcp_requests.clone();
        tokio::spawn(async move {
            let (mut stream, _) = tcp.accept().await.unwrap();
            tcp_requests.fetch_add(1, Ordering::SeqCst);
            let mut len_buf = [0u8; 2];
            stream.read_exact(&mut len_buf).await.unwrap();
            let req_len = u16::from_be_bytes(len_buf) as usize;
            let mut buf = vec![0u8; req_len];
            stream.read_exact(&mut buf).await.unwrap();
            let message = copy_into_packet(&buf);
            let question = message.question.first().unwrap();
            let response = make_response(
                message.header.id,
                &question.qname,
                DnsRecord::A {
                    domain: question.qname.clone(),
                    addr: Ipv4Addr::new(8, 8, 8, 8),
                    ttl: 200,
                },
            );
            let resp_len = (response.len() as u16).to_be_bytes();
            stream.write_all(&resp_len).await.unwrap();
            stream.write_all(&response).await.unwrap();
        })
    };

    let resolver = DnsResolver::new(addr, Duration::from_millis(500));
    let resolution = resolver
        .resolve("truncated.example")
        .await
        .expect("resolution succeeded");
    assert_eq!(resolution.a_records[0].addr, Ipv4Addr::new(8, 8, 8, 8));
    assert_eq!(udp_requests.load(Ordering::SeqCst), 1);
    assert_eq!(tcp_requests.load(Ordering::SeqCst), 1);

    udp_task.await.unwrap();
    tcp_task.await.unwrap();
}

#[tokio::test]
async fn resolver_obeys_ttl_cache() {
    let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = udp.local_addr().unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    let udp_task = {
        let requests = requests.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            loop {
                let (len, peer) = udp.recv_from(&mut buf).await.unwrap();
                requests.fetch_add(1, Ordering::SeqCst);
                let message = copy_into_packet(&buf[..len]);
                let question = message.question.first().unwrap();
                let response = match question.qtype {
                    QueryType::A => make_response(
                        message.header.id,
                        &question.qname,
                        DnsRecord::A {
                            domain: question.qname.clone(),
                            addr: Ipv4Addr::new(9, 9, 9, 9),
                            ttl: 1,
                        },
                    ),
                    QueryType::AAAA => make_response(
                        message.header.id,
                        &question.qname,
                        DnsRecord::AAAA {
                            domain: question.qname.clone(),
                            addr: Ipv6Addr::LOCALHOST,
                            ttl: 300,
                        },
                    ),
                    QueryType::MX => make_response(
                        message.header.id,
                        &question.qname,
                        DnsRecord::MX {
                            domain: question.qname.clone(),
                            preference: 5,
                            exchange: "mx.ttl.example".to_string(),
                            ttl: 300,
                        },
                    ),
                    QueryType::NS => make_response(
                        message.header.id,
                        &question.qname,
                        DnsRecord::NS {
                            domain: question.qname.clone(),
                            host: "ns.ttl.example".to_string(),
                            ttl: 300,
                        },
                    ),
                    _ => make_response(
                        message.header.id,
                        &question.qname,
                        DnsRecord::TXT {
                            domain: question.qname.clone(),
                            data: vec!["ttl".to_string()],
                            ttl: 300,
                        },
                    ),
                };
                udp.send_to(&response, peer).await.unwrap();
                if requests.load(Ordering::SeqCst) >= 10 {
                    break;
                }
            }
        })
    };

    let resolver = DnsResolver::new(addr, Duration::from_millis(500));
    resolver
        .resolve("ttl.example")
        .await
        .expect("first resolve succeeded");
    tokio::time::sleep(Duration::from_millis(1100)).await;
    resolver
        .resolve("ttl.example")
        .await
        .expect("second resolve succeeded");
    assert!(requests.load(Ordering::SeqCst) >= 10);

    udp_task.abort();
}

#[tokio::test]
async fn resolver_deduplicates_singleflight() {
    let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = udp.local_addr().unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    let notify = Arc::new(Notify::new());
    let proceed = Arc::new(Notify::new());
    let udp_task = {
        let requests = requests.clone();
        let notify = notify.clone();
        let proceed = proceed.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            let (len, peer) = udp.recv_from(&mut buf).await.unwrap();
            requests.fetch_add(1, Ordering::SeqCst);
            notify.notify_waiters();
            proceed.notified().await;
            let message = copy_into_packet(&buf[..len]);
            let question = message.question.first().unwrap();
            let response = make_response(
                message.header.id,
                &question.qname,
                DnsRecord::A {
                    domain: question.qname.clone(),
                    addr: Ipv4Addr::new(7, 7, 7, 7),
                    ttl: 300,
                },
            );
            udp.send_to(&response, peer).await.unwrap();
        })
    };

    let resolver = DnsResolver::new(addr, Duration::from_millis(500));
    let resolver_clone = resolver.clone();
    let task1 = tokio::spawn(async move { resolver.resolve("singleflight.example").await });
    notify.notified().await;
    let task2 = tokio::spawn(async move { resolver_clone.resolve("singleflight.example").await });
    proceed.notify_waiters();
    let result1 = task1.await.unwrap().unwrap();
    let result2 = task2.await.unwrap().unwrap();
    assert_eq!(result1.a_records[0].addr, Ipv4Addr::new(7, 7, 7, 7));
    assert_eq!(result2.a_records[0].addr, Ipv4Addr::new(7, 7, 7, 7));
    assert_eq!(requests.load(Ordering::SeqCst), 1);

    udp_task.await.unwrap();
}
