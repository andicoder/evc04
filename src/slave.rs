//! The Modbus-RTU slave loop: read polls off the gateway socket and answer them.
//!
//! We speak raw RTU over a plain TCP socket we open *to* the transparent
//! RS485↔TCP gateway (SPECS.md §3) — no MBAP header, no `tokio-modbus` server.
//! The framing lives in [`crate::frame`] so the exact §5/§11 bytes stay verified.

use crate::frame::{build_response, encode_currents, parse_request};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpStream, ToSocketAddrs};

/// Every request the EVC04 issues is a fixed 8-byte read frame (SPECS.md §4):
/// `addr | fc | start(2) | qty(2) | crc(2)`.
const REQUEST_LEN: usize = 8;

/// The request that counts as *our* meter poll (SPECS.md §4). Defaults match the
/// EVC04 (addr 1, register `0x500C`, qty 6); env config (§7) overrides them later.
#[derive(Debug, Clone, Copy)]
pub struct PollMatch {
    pub addr: u8,
    pub register: u16,
    pub qty: u16,
}

impl Default for PollMatch {
    fn default() -> Self {
        Self {
            addr: 1,
            register: 0x500C,
            qty: 6,
        }
    }
}

/// Serve poll responses over an already-connected stream until the peer closes.
pub async fn serve_connection<S>(
    mut stream: S,
    poll: PollMatch,
    currents: impl Fn() -> [f32; 3],
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // TCP gives no frame boundaries, so buffer bytes and pull whole 8-byte frames.
    let mut buf: Vec<u8> = Vec::with_capacity(REQUEST_LEN * 2);
    let mut chunk = [0u8; 256];
    loop {
        while buf.len() >= REQUEST_LEN {
            match parse_request(&buf[..REQUEST_LEN]) {
                Some(req) if req.is_poll(poll.addr, poll.register, poll.qty) => {
                    let [l1, l2, l3] = currents();
                    let response = build_response(req.addr, &encode_currents(l1, l2, l3));
                    stream.write_all(&response).await?;
                    buf.drain(..REQUEST_LEN);
                }
                // Well-formed frame that isn't our poll: drop it, stay silent.
                Some(_) => {
                    buf.drain(..REQUEST_LEN);
                }
                // Bad CRC / misalignment: slide one byte to resync rather than
                // discarding a whole frame's worth — keeps us from a resync storm.
                None => {
                    buf.drain(..1);
                }
            }
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(()); // peer closed the socket
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Open a TCP socket to the gateway and serve poll responses over it.
pub async fn connect_and_serve<A: ToSocketAddrs>(
    addr: A,
    poll: PollMatch,
    currents: impl Fn() -> [f32; 3],
) -> std::io::Result<()> {
    let stream = TcpStream::connect(addr).await?;
    serve_connection(stream, poll, currents).await
}
