pub mod concurrency;
pub mod control_plane;
pub mod dispatcher;
pub mod lease_extender;
pub mod resolver;
pub mod runtime;
pub mod workers;

use anyhow::{anyhow, Ok, Result};
use bytes::BytesMut;
use crossbeam_queue::SegQueue;
use once_cell::sync::Lazy;
use std::{
    mem,
    net::{Ipv4Addr, Ipv6Addr},
};

pub const MAX_PACKET_SIZE: usize = 1232;

static BYTE_PACKET_POOL: Lazy<PacketPool> = Lazy::new(PacketPool::default);

struct PacketPool {
    buffers: SegQueue<BytesMut>,
}

impl Default for PacketPool {
    fn default() -> Self {
        Self {
            buffers: SegQueue::new(),
        }
    }
}

impl PacketPool {
    fn get(&self) -> BytesMut {
        self.buffers.pop().unwrap_or_else(|| {
            let mut buf = BytesMut::with_capacity(MAX_PACKET_SIZE);
            buf.resize(MAX_PACKET_SIZE, 0);
            buf
        })
    }

    fn put(&self, mut buffer: BytesMut) {
        if buffer.len() != MAX_PACKET_SIZE {
            buffer.resize(MAX_PACKET_SIZE, 0);
        }
        buffer[..].fill(0);
        self.buffers.push(buffer);
    }
}

pub struct BytePacketBuf {
    pub buf: BytesMut,
    pub pos: usize,
    pub len: usize,
}

impl BytePacketBuf {
    pub fn new() -> BytePacketBuf {
        let mut buf = BYTE_PACKET_POOL.get();
        if buf.len() != MAX_PACKET_SIZE {
            buf.resize(MAX_PACKET_SIZE, 0);
        }
        buf[..].fill(0);
        BytePacketBuf {
            buf,
            pos: 0,
            len: 0,
        }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    fn ensure_capacity(&self, end: usize) -> Result<()> {
        if end > self.buf.len() {
            return Err(anyhow!("End of buffer"));
        }
        Ok(())
    }

    fn ensure_readable(&self, end: usize) -> Result<()> {
        if end > self.len {
            return Err(anyhow!("End of buffer"));
        }
        Ok(())
    }

    fn get_range(&mut self, start: usize, len: usize) -> Result<&[u8]> {
        self.ensure_readable(start + len)?;
        Ok(&self.buf[start..start + len])
    }

    #[allow(dead_code)]
    fn set_to_pos(&mut self, pos: usize) -> Result<()> {
        self.ensure_capacity(pos)?;
        self.pos = pos;
        if self.pos > self.len {
            self.len = self.pos;
        }
        Ok(())
    }

    fn step(&mut self, step: usize) -> Result<()> {
        let new_pos = self.pos + step;
        self.ensure_readable(new_pos)?;
        self.pos = new_pos;
        Ok(())
    }

    pub fn seek(&mut self, pos: usize) -> Result<()> {
        self.ensure_capacity(pos)?;
        self.pos = pos;

        Ok(())
    }

    fn get(&mut self, pos: usize) -> Result<u8> {
        self.ensure_readable(pos + 1)?;
        Ok(self.buf[pos])
    }

    fn read(&mut self) -> Result<u8> {
        self.ensure_readable(self.pos + 1)?;
        let res = self.buf[self.pos];
        self.pos += 1;

        Ok(res)
    }

    fn read_u16(&mut self) -> Result<u16> {
        let res = ((self.read()? as u16) << 8) | (self.read()? as u16);

        Ok(res)
    }

    #[allow(clippy::identity_op)]
    fn read_u32(&mut self) -> Result<u32> {
        let res = ((self.read()? as u32) << 24)
            | ((self.read()? as u32) << 16)
            | ((self.read()? as u32) << 8)
            | ((self.read()? as u32) << 0);

        Ok(res)
    }

    #[allow(clippy::identity_op)]
    fn read_ipv4(&mut self) -> Result<Ipv4Addr> {
        let ip = self.read_u32()?;
        Ok(Ipv4Addr::new(
            ((ip >> 24) & 0xFF) as u8,
            ((ip >> 16) & 0xFF) as u8,
            ((ip >> 8) & 0xFF) as u8,
            ((ip >> 0) & 0xFF) as u8,
        ))
    }

    fn read_ipv6(&mut self) -> Result<Ipv6Addr> {
        let mut segments = [0u16; 8];
        for segment in &mut segments {
            *segment = self.read_u16()?;
        }
        Ok(Ipv6Addr::new(
            segments[0],
            segments[1],
            segments[2],
            segments[3],
            segments[4],
            segments[5],
            segments[6],
            segments[7],
        ))
    }

    pub fn read_qname(&mut self, outstr: &mut String) -> Result<()> {
        let mut pos = self.pos();
        let mut jumped = false;
        let max_jumps = 10;
        let mut jumps_prfmd = 0;
        let mut delim = "";

        loop {
            if jumps_prfmd > max_jumps {
                return Err(anyhow!("Limit of {} jumps exceeded", max_jumps));
            }
            // как content length только для lebal
            let len = self.get(pos)?;

            // If len has the two most significant bit are set, it represents a
            // jump to some other offset in the packet:
            if (len & 0xC0) == 0xC0 {
                // Update the buffer position to a point past the current
                // label. We don't need to touch it any further.
                if !jumped {
                    self.seek(pos + 2)?;
                }

                let b2 = self.get(pos + 1)? as u16;
                let offset = (((len as u16) ^ 0xC0) << 8) | b2;
                pos = offset as usize;

                // Indicate that a jump was performed.
                jumped = true;
                jumps_prfmd += 1;

                continue;
            } else {
                // Move a single byte forward to move past the length byte.
                pos += 1;

                // Domain names are terminated by an empty label of length 0,
                // so if the length is zero we're done.
                if len == 0 {
                    break;
                }

                // Append the delimiter to our output buffer first.
                outstr.push_str(delim); // добавляет пустой байт в конец, чтобы его указать)

                let str_buffer = self.get_range(pos, len as usize)?;
                outstr.push_str(&String::from_utf8_lossy(str_buffer).to_lowercase());

                delim = ".";

                // Move forward the full length of the label.
                pos += len as usize;
            }
        }
        if !jumped {
            self.seek(pos)?;
        }
        Ok(())
    }

    pub fn write_u8(&mut self, val: u8) -> Result<()> {
        self.ensure_capacity(self.pos + 1)?;
        self.buf[self.pos] = val;
        self.pos += 1;
        if self.pos > self.len {
            self.len = self.pos;
        }
        Ok(())
    }
    // сделать write интерфейс, по дженерик типу. вставил
    // turbofish и погнал. функция сама поставит self.pos
    // на необходимое место
    // - прописать за что отвечает каждый бит в пакете
    // - ебать это будет удобно
    #[allow(clippy::identity_op)]
    pub fn write_u16(&mut self, val: u16) -> Result<()> {
        self.write_u8(((val >> 8) & 0xff) as u8)?;
        self.write_u8(((val >> 0) & 0xff) as u8)?;
        Ok(())
    }
    #[allow(clippy::identity_op)]
    pub fn write_u32(&mut self, val: u32) -> Result<()> {
        self.write_u8(((val >> 24) & 0xff) as u8)?;
        self.write_u8(((val >> 16) & 0xff) as u8)?;
        self.write_u8(((val >> 8) & 0xff) as u8)?;
        self.write_u8(((val >> 0) & 0xff) as u8)?;
        Ok(())
    }

    pub fn write_qname(&mut self, qname: &str) -> Result<()> {
        for label in qname.split('.').filter(|label| label.len() < 64) {
            self.write_u8(label.len() as u8)?;
            for byte in label.as_bytes() {
                self.write_u8(*byte)?;
            }
        }
        self.write_u8(0)?;
        Ok(())
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        self.buf.as_mut()
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    pub fn set_len(&mut self, len: usize) -> Result<()> {
        if len > self.buf.len() {
            return Err(anyhow!("length exceeds buffer"));
        }
        self.len = len;
        if self.pos > self.len {
            self.pos = self.len;
        }
        Ok(())
    }
}

impl Default for BytePacketBuf {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for BytePacketBuf {
    fn drop(&mut self) {
        let mut buffer = BytesMut::new();
        mem::swap(&mut self.buf, &mut buffer);
        BYTE_PACKET_POOL.put(buffer);
        self.pos = 0;
        self.len = 0;
    }
}

#[rustfmt::skip]
#[derive(Debug)]
pub struct Dns {
    pub header:       DnsHeader,
    pub question:     Vec<Question>,
    pub answer:       Vec<DnsRecord>,
    pub authority:    Vec<DnsRecord>,
    pub additional:   Vec<DnsRecord>,
}

impl Default for Dns {
    fn default() -> Self {
        Self::new()
    }
}

impl Dns {
    pub fn new() -> Self {
        Self {
            header: DnsHeader::new(),
            question: Vec::new(),
            answer: Vec::new(),
            authority: Vec::new(),
            additional: Vec::new(),
        }
    }

    pub fn parse_req(buffer: &mut BytePacketBuf) -> Result<Self> {
        let mut result = Dns::new();
        result.header.read(buffer)?;

        for _ in 0..result.header.qdcount {
            let mut question = Question {
                qname: "".to_string(),
                qtype: QueryType::UNKNOWN(0),
            };
            question.read(buffer)?;
            result.question.push(question);
        }

        for _ in 0..result.header.ancount {
            let answer = DnsRecord::read(buffer)?;
            result.answer.push(answer);
        }

        for _ in 0..result.header.nscount {
            let authority = DnsRecord::read(buffer)?;
            result.authority.push(authority);
        }

        for _ in 0..result.header.arcount {
            let additional = DnsRecord::read(buffer)?;
            result.additional.push(additional);
        }
        Ok(result)
    }

    pub fn write_to_buf(self: &mut Dns, buffer: &mut BytePacketBuf) -> Result<()> {
        buffer.write_u16(self.header.id)?;
        buffer.write_u16(self.header.flags.into())?;
        buffer.write_u16(self.header.qdcount)?;
        buffer.write_u16(self.header.ancount)?;
        buffer.write_u16(self.header.nscount)?;
        buffer.write_u16(self.header.arcount)?;
        Ok(())

        // сравнение пакетов с действительностью владеющих данных и возможностей
        // формирование ответа
        // порядок записи
        // методы для записи(то же самое как с чтением)
        // кидать днс запрос гуглу(самостоятельно его создавать)
        // от него прочитывать и отдавать пользователю
        // или сразу передать его без чтения
    }
}

// попробовать каунтеры в отдельную структуру засунуть,
// а эту сделать repr(64) и repr(C). посмотреть на перфоманс
#[rustfmt::skip]
#[derive(Debug)]
pub struct DnsHeader {      //                         96  bits == 12 Bytes 
    pub id:        u16,     //                         16  bits
    pub flags:     Flags,   //                         16  bits
    pub qdcount:   u16,     //    Question Count       16  bits
    pub ancount:   u16,     //    Answer Count         16  bits
    pub nscount:   u16,     //    Authority Count      16  bits
    pub arcount:   u16,     //    Additional Count     16  bits
}

#[rustfmt::skip]
impl DnsHeader {
    pub fn new() -> Self {
        Self { 
            id: 0,
            flags: Flags::new(),
            qdcount: 0,
            ancount: 0,
            nscount: 0,
            arcount: 0,
        }
    }
    pub fn read(&mut self, buffer: &mut BytePacketBuf) -> Result<()> {
        self.id      = buffer.read_u16()?;
        self.flags   = buffer.read_u16()?.into();
        self.qdcount = buffer.read_u16()?;
        self.ancount = buffer.read_u16()?;
        self.nscount = buffer.read_u16()?;
        self.arcount = buffer.read_u16()?;
        Ok(())
    }
}

impl Default for DnsHeader {
    fn default() -> Self {
        Self::new()
    }
}

#[rustfmt::skip]
#[derive(Debug, Copy, Clone)]
pub struct Flags {                           // 16 bits

    pub response:               bool,        // 1 bit
    pub opcode:                 Opcode,      // 4 bits
    pub authoritative_answer:   bool,        // 1 bit
    pub truncated_message:      bool,        // 1 bit 
    pub recursion_desired:      bool,        // 1 bit

    pub recursion_available:    bool,        // 1 bit
    pub z:                      u8,          // 3 bit
    pub rescode:                ResultCode,  // 4 bits

}

impl std::fmt::Display for Flags {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "response: {}", self.response)?;
        writeln!(f, "opcode: {:?}", self.opcode)?;
        writeln!(f, "authoritative_answer: {}", self.authoritative_answer)?;
        writeln!(f, "truncated_message: {}", self.truncated_message)?;
        writeln!(f, "recursion_desired: {}", self.recursion_desired)?;
        writeln!(f, "recursion_available: {}", self.recursion_available)?;
        writeln!(f, "z: {}", self.z)?;
        writeln!(f, "rescode: {:?}", self.rescode)
    }
}

impl Flags {
    fn new() -> Self {
        Self {
            response: false,
            opcode: Opcode::Query,
            authoritative_answer: false,
            truncated_message: false,
            recursion_desired: false,

            recursion_available: false,
            z: 0,
            rescode: ResultCode::NOERROR,
        }
    }
}

impl From<u16> for Flags {
    fn from(value: u16) -> Self {
        Self {
            response: (&value & 0x8000) != 0,
            opcode: (((&value & 0x7800) >> 11) as u8).into(),
            authoritative_answer: (&value & 0x0400) != 0,
            truncated_message: (value & 0x200) != 0,

            recursion_desired: (&value & 0x100) != 0,
            recursion_available: (&value & 0x80) != 0,
            z: ((&value & 0x5) >> 4) as u8,
            rescode: (value & 0xF).into(),
        }
    }
}

impl From<Flags> for u16 {
    fn from(value: Flags) -> Self {
        value.into()
    }
}

#[rustfmt::skip]
#[derive(Debug, PartialEq, Copy, Clone)]
#[repr(u8)]

// попробовать вытаскивать по дискриминанту с ансейфом - смотреть обсидиан
pub enum Opcode {
    Query,
    IQuery,
    Status,
    Notify,
    Update,
    Unknown(u8),
}

impl Opcode {
    pub fn discriminant(&self) -> u8 {
        // SAFETY: беру первый байт объекта enum,
        // который отвечает за сопоставление
        // - дискриминант
        // пока экспериментальый функционал. посмотрим пригодится ли.
        // а так, ощущается как сложность ради сложности. но не вижу в этом
        // ничего плохого
        unsafe { *<*const _>::from(self).cast::<u8>() }
    }
}

impl From<u8> for Opcode {
    fn from(opcode: u8) -> Opcode {
        match opcode {
            0 => Opcode::Query,
            1 => Opcode::IQuery,
            2 => Opcode::Status,
            4 => Opcode::Notify,
            5 => Opcode::Update,
            v => Opcode::Unknown(v),
        }
    }
}

#[rustfmt::skip]
impl From<Opcode> for u8 {
    fn from(op: Opcode) -> u8 {
        Opcode::discriminant(&op)
    }
}

#[rustfmt::skip]
#[derive(Debug, PartialEq, Copy, Clone)]
// первый байт и так говорит о позиции объекта enum
// пошаманить с fn дискриминант TODO
pub enum ResultCode {
    NOERROR  = 0,
    FORMERR  = 1,
    SERVFAIL = 2,
    NXDOMAIN = 3,
    NOTIMP   = 4,
    REFUSED  = 5,
}

impl From<u16> for ResultCode {
    fn from(num: u16) -> ResultCode {
        match num {
            1 => ResultCode::FORMERR,
            2 => ResultCode::SERVFAIL,
            3 => ResultCode::NXDOMAIN,
            4 => ResultCode::NOTIMP,
            5 => ResultCode::REFUSED,
            0 | _ => ResultCode::NOERROR,
        }
    }
}

#[rustfmt::skip]
impl From<ResultCode> for u16 {
    fn from(rescode: ResultCode) -> u16 {
        match rescode {
            ResultCode::FORMERR  => 1,
            ResultCode::SERVFAIL => 2,
            ResultCode::NXDOMAIN => 3,
            ResultCode::NOTIMP   => 4,
            ResultCode::REFUSED  => 5,
            ResultCode::NOERROR  => 0,
        }
    }
}

#[rustfmt::skip]
#[derive(Debug)]
pub struct Question {
    pub qname:  String,
    pub qtype:  QueryType,
}

impl Question {
    pub fn new(name: String, qtype: QueryType) -> Question {
        Question { qname: name, qtype }
    }
    pub fn read(&mut self, buffer: &mut BytePacketBuf) -> Result<()> {
        buffer.read_qname(&mut self.qname)?;
        self.qtype = buffer.read_u16()?.into();
        let _ = buffer.read_u16()?; // qclass
        Ok(())
    }
}

#[allow(dead_code)]
#[derive(Debug)]
pub struct ResourceRecord {
    name: String,
    rtype: QueryType,
    class: u16,
    ttl: u32,
    len: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum QueryType {
    UNKNOWN(u16),
    A,
    AAAA,
    NS,
    MX,
    TXT,
    OPT,
}

impl From<u16> for QueryType {
    fn from(value: u16) -> Self {
        match value {
            1 => QueryType::A,
            2 => QueryType::NS,
            15 => QueryType::MX,
            16 => QueryType::TXT,
            28 => QueryType::AAAA,
            41 => QueryType::OPT,
            _ => QueryType::UNKNOWN(value),
        }
    }
}

impl From<QueryType> for u16 {
    fn from(value: QueryType) -> Self {
        match value {
            QueryType::A => 1,
            QueryType::NS => 2,
            QueryType::MX => 15,
            QueryType::TXT => 16,
            QueryType::AAAA => 28,
            QueryType::OPT => 41,
            QueryType::UNKNOWN(x) => x,
        }
    }
}

#[derive(Debug, Clone)]
pub enum DnsRecord {
    UNKNOWN {
        domain: String,
        rtype: QueryType,
        data_len: u16,
        ttl: u32,
    },
    A {
        domain: String,
        addr: Ipv4Addr,
        ttl: u32,
    },
    AAAA {
        domain: String,
        addr: Ipv6Addr,
        ttl: u32,
    },
    NS {
        domain: String,
        host: String,
        ttl: u32,
    },
    MX {
        domain: String,
        preference: u16,
        exchange: String,
        ttl: u32,
    },
    TXT {
        domain: String,
        data: Vec<String>,
        ttl: u32,
    },
    OPT {
        payload_size: u16,
        extended_rcode: u8,
        version: u8,
        flags: u16,
        options: Vec<u8>,
    },
}

impl DnsRecord {
    pub fn read(buffer: &mut BytePacketBuf) -> Result<DnsRecord> {
        let mut domain = String::new();
        buffer.read_qname(&mut domain)?;
        let rtype: QueryType = buffer.read_u16()?.into();
        let class = buffer.read_u16()?;
        let ttl = buffer.read_u32()?;
        let data_len = buffer.read_u16()?;

        match rtype {
            QueryType::A => {
                let addr = buffer.read_ipv4()?;
                Ok(DnsRecord::A { domain, addr, ttl })
            }
            QueryType::AAAA => {
                let addr = buffer.read_ipv6()?;
                Ok(DnsRecord::AAAA { domain, addr, ttl })
            }
            QueryType::NS => {
                let mut host = String::new();
                let start = buffer.pos();
                buffer.read_qname(&mut host)?;
                let consumed = buffer.pos() - start;
                if consumed < data_len as usize {
                    buffer.step(data_len as usize - consumed)?;
                }
                Ok(DnsRecord::NS { domain, host, ttl })
            }
            QueryType::MX => {
                let pref = buffer.read_u16()?;
                let mut exchange = String::new();
                let start = buffer.pos();
                buffer.read_qname(&mut exchange)?;
                let consumed = 2 + (buffer.pos() - start);
                if consumed < data_len as usize {
                    buffer.step(data_len as usize - consumed)?;
                }
                Ok(DnsRecord::MX {
                    domain,
                    preference: pref,
                    exchange,
                    ttl,
                })
            }
            QueryType::TXT => {
                let mut remaining = data_len as usize;
                let mut entries = Vec::new();
                while remaining > 0 {
                    let txt_len = buffer.read()? as usize;
                    remaining = remaining
                        .checked_sub(1)
                        .ok_or_else(|| anyhow!("invalid TXT length"))?;
                    if txt_len > remaining {
                        return Err(anyhow!("invalid TXT record length"));
                    }
                    let mut data = Vec::with_capacity(txt_len);
                    for _ in 0..txt_len {
                        data.push(buffer.read()?);
                    }
                    remaining -= txt_len;
                    entries.push(String::from_utf8_lossy(&data).into_owned());
                }
                Ok(DnsRecord::TXT {
                    domain,
                    data: entries,
                    ttl,
                })
            }
            QueryType::OPT => {
                let extended_rcode = ((ttl >> 24) & 0xFF) as u8;
                let version = ((ttl >> 16) & 0xFF) as u8;
                let flags = (ttl & 0xFFFF) as u16;
                let mut options = vec![0u8; data_len as usize];
                for byte in options.iter_mut() {
                    *byte = buffer.read()?;
                }
                Ok(DnsRecord::OPT {
                    payload_size: class,
                    extended_rcode,
                    version,
                    flags,
                    options,
                })
            }
            QueryType::UNKNOWN(_) => {
                buffer.step(data_len.into())?;
                Ok(DnsRecord::UNKNOWN {
                    domain,
                    rtype,
                    data_len,
                    ttl,
                })
            }
        }
    }

    pub fn write(&self, buffer: &mut BytePacketBuf) -> Result<usize> {
        let start_pos = buffer.pos;

        match self {
            DnsRecord::A { domain, addr, ttl } => {
                buffer.write_qname(domain)?;
                buffer.write_u16(QueryType::A.into())?;
                buffer.write_u16(1)?;
                buffer.write_u32(*ttl)?;
                buffer.write_u16(4)?;

                let octets = addr.octets();
                for byte in &octets {
                    buffer.write_u8(*byte)?;
                }
            }
            DnsRecord::AAAA { domain, addr, ttl } => {
                buffer.write_qname(domain)?;
                buffer.write_u16(QueryType::AAAA.into())?;
                buffer.write_u16(1)?;
                buffer.write_u32(*ttl)?;
                buffer.write_u16(16)?;
                for segment in &addr.octets() {
                    buffer.write_u8(*segment)?;
                }
            }
            DnsRecord::NS { domain, host, ttl } => {
                buffer.write_qname(domain)?;
                buffer.write_u16(QueryType::NS.into())?;
                buffer.write_u16(1)?;
                buffer.write_u32(*ttl)?;
                let rdlength_pos = buffer.pos();
                buffer.write_u16(0)?;
                let data_start = buffer.pos();
                buffer.write_qname(host)?;
                let rdata_len = buffer.pos() - data_start;
                let current_pos = buffer.pos();
                buffer.seek(rdlength_pos)?;
                buffer.write_u16(rdata_len as u16)?;
                buffer.seek(current_pos)?;
            }
            DnsRecord::MX {
                domain,
                preference,
                exchange,
                ttl,
            } => {
                buffer.write_qname(domain)?;
                buffer.write_u16(QueryType::MX.into())?;
                buffer.write_u16(1)?;
                buffer.write_u32(*ttl)?;
                let rdlength_pos = buffer.pos();
                buffer.write_u16(0)?;
                let data_start = buffer.pos();
                buffer.write_u16(*preference)?;
                buffer.write_qname(exchange)?;
                let rdata_len = buffer.pos() - data_start;
                let current_pos = buffer.pos();
                buffer.seek(rdlength_pos)?;
                buffer.write_u16(rdata_len as u16)?;
                buffer.seek(current_pos)?;
            }
            DnsRecord::TXT { domain, data, ttl } => {
                buffer.write_qname(domain)?;
                buffer.write_u16(QueryType::TXT.into())?;
                buffer.write_u16(1)?;
                buffer.write_u32(*ttl)?;
                let rdlength_pos = buffer.pos();
                buffer.write_u16(0)?;
                let data_start = buffer.pos();
                for entry in data {
                    let bytes = entry.as_bytes();
                    buffer.write_u8(bytes.len() as u8)?;
                    for byte in bytes {
                        buffer.write_u8(*byte)?;
                    }
                }
                let rdata_len = buffer.pos() - data_start;
                let current_pos = buffer.pos();
                buffer.seek(rdlength_pos)?;
                buffer.write_u16(rdata_len as u16)?;
                buffer.seek(current_pos)?;
            }
            DnsRecord::UNKNOWN { .. } | DnsRecord::OPT { .. } => {}
        }

        Ok(buffer.pos() - start_pos)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    #[test]
    fn flags_parser() {
        let payload: u16 = 0x0100;
        let pars5d = Flags::from(payload);
        assert!(!pars5d.response);
        assert_eq!(pars5d.opcode, Opcode::Query);
        assert_eq!(pars5d.authoritative_answer, false);
        assert_eq!(pars5d.truncated_message, false);

        assert_eq!(pars5d.recursion_desired, true);
        assert_eq!(pars5d.recursion_available, false);
        assert_eq!(pars5d.z, 0);
        assert_eq!(pars5d.rescode, ResultCode::NOERROR);
    }
}
