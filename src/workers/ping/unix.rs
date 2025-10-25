use std::io;
use std::mem::MaybeUninit;
use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use pnet_packet::icmp::{
    destination_unreachable::DestinationUnreachablePacket, echo_reply::EchoReplyPacket,
    echo_request::MutableEchoRequestPacket, time_exceeded::TimeExceededPacket, IcmpCode,
    IcmpPacket, IcmpTypes,
};
use pnet_packet::ip::IpNextHeaderProtocols;
use pnet_packet::ipv4::Ipv4Packet;
use pnet_packet::util::checksum;
use pnet_packet::Packet;
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use tokio::io::unix::AsyncFd;
use tokio::time::{self, Instant};
use tokio_util::sync::CancellationToken;

use super::IcmpOutcome;

pub struct IcmpSocket {
    inner: AsyncFd<Socket>,
}

impl IcmpSocket {
    pub fn new(address: &SocketAddr) -> Result<Self> {
        match address {
            SocketAddr::V4(_) => Self::new_v4(),
            SocketAddr::V6(_) => Err(anyhow!("IPv6 ICMP is not supported")),
        }
    }

    fn new_v4() -> Result<Self> {
        let socket = Socket::new(Domain::IPV4, Type::RAW, Some(Protocol::ICMPV4))
            .context("failed to create ICMP socket")?;
        socket
            .set_nonblocking(true)
            .context("failed to configure ICMP socket as non-blocking")?;
        let async_fd = AsyncFd::new(socket).context("failed to wrap ICMP socket in AsyncFd")?;
        Ok(Self { inner: async_fd })
    }

    pub async fn send_echo(
        &self,
        address: &SocketAddr,
        identifier: u16,
        sequence: u16,
        payload: &[u8],
        timeout: Duration,
        cancel: CancellationToken,
    ) -> Result<IcmpOutcome> {
        let target = SockAddr::from(*address);
        let mut packet = vec![0u8; 8 + payload.len()];
        {
            let mut echo = MutableEchoRequestPacket::new(&mut packet)
                .context("failed to construct echo request packet")?;
            echo.set_icmp_type(IcmpTypes::EchoRequest);
            echo.set_icmp_code(IcmpCode::new(0));
            echo.set_identifier(identifier);
            echo.set_sequence_number(sequence);
            echo.set_payload(payload);
            let checksum = checksum(echo.packet(), 1);
            echo.set_checksum(checksum);
        }

        let start = Instant::now();
        self.send_to(&packet, &target).await?;
        self.recv_response(identifier, sequence, start, timeout, cancel)
            .await
    }

    async fn send_to(&self, packet: &[u8], target: &SockAddr) -> Result<()> {
        loop {
            let mut guard = self
                .inner
                .writable()
                .await
                .context("ICMP socket became unwritable")?;
            match guard.try_io(|inner| inner.get_ref().send_to(packet, target)) {
                Ok(Ok(_)) => return Ok(()),
                Ok(Err(err)) if err.kind() == io::ErrorKind::WouldBlock => continue,
                Ok(Err(err)) => return Err(err.into()),
                Err(_would_block) => continue,
            }
        }
    }

    async fn recv_response(
        &self,
        identifier: u16,
        sequence: u16,
        start: Instant,
        timeout: Duration,
        cancel: CancellationToken,
    ) -> Result<IcmpOutcome> {
        let deadline = start + timeout;
        let mut buffer = vec![MaybeUninit::<u8>::uninit(); 2048];

        loop {
            if cancel.is_cancelled() {
                return Ok(IcmpOutcome::Cancelled);
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(IcmpOutcome::Timeout);
            }
            let remaining = deadline.saturating_duration_since(now);
            let readable = tokio::select! {
                _ = cancel.cancelled() => return Ok(IcmpOutcome::Cancelled),
                result = time::timeout(remaining, self.inner.readable()) => result,
            };

            let mut guard = match readable {
                Ok(Ok(guard)) => guard,
                Ok(Err(err)) => return Err(err.into()),
                Err(_) => return Ok(IcmpOutcome::Timeout),
            };

            let result = guard.try_io(|inner| inner.get_ref().recv_from(&mut buffer));
            let (size, _addr) = match result {
                Ok(Ok((size, _addr))) => (size, _addr),
                Ok(Err(err)) if err.kind() == io::ErrorKind::WouldBlock => continue,
                Ok(Err(err)) => return Err(err.into()),
                Err(_would_block) => continue,
            };

            let filled = unsafe { std::slice::from_raw_parts(buffer.as_ptr() as *const u8, size) };

            if let Some(outcome) = parse_response(filled, identifier, sequence, start) {
                return Ok(outcome);
            }
        }
    }
}

fn parse_response(
    data: &[u8],
    identifier: u16,
    sequence: u16,
    start: Instant,
) -> Option<IcmpOutcome> {
    let ipv4 = Ipv4Packet::new(data)?;
    if ipv4.get_next_level_protocol() != IpNextHeaderProtocols::Icmp {
        return None;
    }
    let icmp = IcmpPacket::new(ipv4.payload())?;
    match icmp.get_icmp_type() {
        IcmpTypes::EchoReply => {
            let reply = EchoReplyPacket::new(icmp.packet())?;
            if reply.get_identifier() != identifier || reply.get_sequence_number() != sequence {
                return None;
            }
            let elapsed = Instant::now().saturating_duration_since(start);
            let bytes = reply.packet().len();
            Some(IcmpOutcome::Echo {
                rtt: elapsed,
                bytes,
                ttl: Some(ipv4.get_ttl()),
            })
        }
        IcmpTypes::DestinationUnreachable => {
            let packet = DestinationUnreachablePacket::new(icmp.packet())?;
            let code = packet.get_icmp_code().0;
            Some(IcmpOutcome::DestinationUnreachable {
                code,
                message: format!("destination unreachable (code {code})"),
            })
        }
        IcmpTypes::TimeExceeded => {
            let packet = TimeExceededPacket::new(icmp.packet())?;
            let code = packet.get_icmp_code().0;
            Some(IcmpOutcome::Filtered {
                code,
                message: format!("time exceeded (code {code})"),
            })
        }
        _ => None,
    }
}
