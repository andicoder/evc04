//! The Modbus-RTU slave loop: read polls off the gateway socket and answer them.
//!
//! We speak raw RTU over a plain TCP socket we open *to* the transparent
//! RS485↔TCP gateway (SPECS.md §3) — no MBAP header, no `tokio-modbus` server.
//! The framing lives in [`crate::frame`] so the exact §5/§11 bytes stay verified.

use crate::frame::{build_response, encode_currents, parse_request};
use std::io;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpStream, ToSocketAddrs};
use tokio::sync::watch;

/// Every request the EVC04 issues is a fixed 8-byte read frame (SPECS.md §4):
/// `addr | fc | start(2) | qty(2) | crc(2)`.
const REQUEST_LEN: usize = 8;

/// Health of the link to the gateway, published for the status publisher (#5)
/// and the failsafe (#7) to observe (SPECS.md §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkHealth {
    /// Disconnected — connect failed or the peer closed; backing off to retry.
    Down,
    /// Connected and serving polls.
    Up,
    /// Connected but no poll arrived within the stall timeout.
    Stalled,
}

/// Tuning for the resilient link supervisor.
#[derive(Debug, Clone, Copy)]
pub struct LinkConfig {
    pub poll: PollMatch,
    /// Treat the link as stalled if no byte arrives within this window. SPECS.md
    /// §7 wants N× the ~1 s poll cadence; the default is 5×.
    pub stall_timeout: Duration,
    pub backoff_initial: Duration,
    pub backoff_max: Duration,
}

impl Default for LinkConfig {
    fn default() -> Self {
        Self {
            poll: PollMatch::default(),
            stall_timeout: Duration::from_secs(5),
            backoff_initial: Duration::from_millis(500),
            backoff_max: Duration::from_secs(30),
        }
    }
}

/// Bounded exponential backoff: doubles each step, capped at `max`, reset on a
/// successful connect.
struct Backoff {
    initial: Duration,
    max: Duration,
    current: Duration,
}

impl Backoff {
    fn new(initial: Duration, max: Duration) -> Self {
        Self {
            initial,
            max,
            current: initial,
        }
    }

    fn reset(&mut self) {
        self.current = self.initial;
    }

    fn next_delay(&mut self) -> Duration {
        let delay = self.current;
        self.current = (self.current * 2).min(self.max);
        delay
    }
}

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
/// With `stall_timeout: Some(t)`, a read that produces no byte within `t` fails
/// with [`io::ErrorKind::TimedOut`] so the supervisor can reconnect.
pub async fn serve_connection<S>(
    mut stream: S,
    poll: PollMatch,
    stall_timeout: Option<Duration>,
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
        let n = match stall_timeout {
            // A read producing no byte within the window means the bus has gone
            // silent — surface it as TimedOut so the supervisor reconnects.
            Some(t) => tokio::time::timeout(t, stream.read(&mut chunk))
                .await
                .map_err(|_| {
                    io::Error::new(io::ErrorKind::TimedOut, "no poll within stall timeout")
                })??,
            None => stream.read(&mut chunk).await?,
        };
        if n == 0 {
            return Ok(()); // peer closed the socket
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Open a TCP socket to the gateway and serve poll responses over it (one shot,
/// no reconnect — see [`run_link`] for the resilient version).
pub async fn connect_and_serve<A: ToSocketAddrs>(
    addr: A,
    poll: PollMatch,
    currents: impl Fn() -> [f32; 3],
) -> std::io::Result<()> {
    let stream = TcpStream::connect(addr).await?;
    serve_connection(stream, poll, None, currents).await
}

/// Keep the gateway link alive 24/7 (SPECS.md §7): connect, serve, and on any
/// disconnect/error/stall back off and reconnect, publishing [`LinkHealth`] for
/// observers. Runs until the task is cancelled.
pub async fn run_link<A>(
    addr: A,
    config: LinkConfig,
    currents: impl Fn() -> [f32; 3],
    health: watch::Sender<LinkHealth>,
) where
    A: ToSocketAddrs + Clone,
{
    let mut backoff = Backoff::new(config.backoff_initial, config.backoff_max);
    // Log only health *edges*, not the per-loop resends — the box polls at 1 Hz, so an
    // unconditional log here would be noise (issue #43).
    let mut last = *health.borrow();
    let mut announce = |to: LinkHealth| {
        if to != last {
            match to {
                LinkHealth::Up => tracing::info!("gateway link up"),
                LinkHealth::Stalled => tracing::warn!("gateway link stalled (no poll in window)"),
                LinkHealth::Down => tracing::warn!("gateway link down"),
            }
            last = to;
        }
        let _ = health.send(to);
    };
    loop {
        match TcpStream::connect(addr.clone()).await {
            Ok(stream) => {
                announce(LinkHealth::Up);
                backoff.reset();
                match serve_connection(stream, config.poll, Some(config.stall_timeout), &currents)
                    .await
                {
                    Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                        announce(LinkHealth::Stalled);
                    }
                    // Clean close (Ok) or any other I/O error: link is down.
                    _ => {
                        announce(LinkHealth::Down);
                    }
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, "gateway connect failed; backing off");
                announce(LinkHealth::Down);
            }
        }
        tokio::time::sleep(backoff.next_delay()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_until_capped() {
        let mut b = Backoff::new(Duration::from_millis(100), Duration::from_millis(400));
        assert_eq!(b.next_delay(), Duration::from_millis(100));
        assert_eq!(b.next_delay(), Duration::from_millis(200));
        assert_eq!(b.next_delay(), Duration::from_millis(400));
        assert_eq!(b.next_delay(), Duration::from_millis(400)); // capped
    }

    #[test]
    fn backoff_resets_to_initial() {
        let mut b = Backoff::new(Duration::from_millis(100), Duration::from_millis(400));
        b.next_delay();
        b.next_delay();
        b.reset();
        assert_eq!(b.next_delay(), Duration::from_millis(100));
    }
}
