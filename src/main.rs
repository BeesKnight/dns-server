use anyhow::Error;
use codecrafters_dns_server::{self, BytePacketBuf, Dns};
#[allow(unused_imports)]
use std::net::UdpSocket;

fn main() -> Result<(), Error> {
    let udp_socket = UdpSocket::bind("127.0.0.1:2053").expect("Failed to bind to address");
    let resolver_socket = UdpSocket::bind("0.0.0.0:0")?;

    loop {
        // хуёвая затея. лучше переписывать TODO
        let mut req_buffer = BytePacketBuf::new();
        let mut res_buffer = BytePacketBuf::new();
        match udp_socket.recv_from(&mut req_buffer.buf) {
            Ok((size, source)) => {
                let request = Dns::parse_req(&mut req_buffer).unwrap();
                resolver_socket.send_to(&req_buffer.buf, "127.0.0.1:5354")?;
                match resolver_socket.recv_from(&mut res_buffer.buf) {
                    Ok(_) => {
                        // в остальном не нуждаемся, за позицией в буфере не следим
                        // id  находится в самом начале, а это дефолтная позиция буфера
                        res_buffer.write_u16(request.header.id)?;
                    },
                    Err(_) => unreachable!(),
                };
                
        
                println!("Received {} bytes from {}", size, source);

                udp_socket
                    .send_to(&res_buffer.buf, source)
                    .expect("Failed to send response");
            }
            Err(e) => {
                eprintln!("Error receiving data: {}", e);
                break Ok(());
            }
        }
    }
}
