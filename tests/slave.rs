//! Integration tests for the RTU-over-TCP serve loop (SPECS.md §3/§5).

use evc04_charge::slave::{connect_and_serve, serve_connection, PollMatch};
use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

// SPECS.md §4 verified poll frame.
const SPEC_POLL: [u8; 8] = [0x01, 0x03, 0x50, 0x0c, 0x00, 0x06, 0x14, 0xcb];

// SPECS.md §5 verified 0 A response (addr 01, FC 03, count 0x0c, 12× 0x00, CRC 93 70).
const ZERO_AMP_RESPONSE: [u8; 17] = [
    0x01, 0x03, 0x0c, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x93, 0x70,
];

fn zero_amps() -> [f32; 3] {
    [0.0, 0.0, 0.0]
}

/// Drive the serve loop with `input`, return everything it writes back. The loop
/// returns once the (shutdown) client half signals EOF, so this is deterministic.
async fn serve_once(input: &[u8]) -> Vec<u8> {
    let (mut client, server) = duplex(1024);
    client.write_all(input).await.unwrap();
    client.shutdown().await.unwrap();
    serve_connection(server, PollMatch::default(), zero_amps)
        .await
        .unwrap();
    let mut out = Vec::new();
    client.read_to_end(&mut out).await.unwrap();
    out
}

#[tokio::test]
async fn answers_the_spec_poll_with_the_zero_amp_frame() {
    assert_eq!(serve_once(&SPEC_POLL).await, ZERO_AMP_RESPONSE);
}

#[tokio::test]
async fn answers_each_poll_independently() {
    // Cadence is content-agnostic (SPECS §4): we answer every poll, no own timer.
    let mut two_polls = SPEC_POLL.to_vec();
    two_polls.extend_from_slice(&SPEC_POLL);
    let mut expected = ZERO_AMP_RESPONSE.to_vec();
    expected.extend_from_slice(&ZERO_AMP_RESPONSE);
    assert_eq!(serve_once(&two_polls).await, expected);
}

#[tokio::test]
async fn resyncs_after_bad_crc_without_storm() {
    // A corrupt frame followed by a good poll must yield exactly one response —
    // a resync storm would emit many.
    let mut corrupt = SPEC_POLL;
    corrupt[7] ^= 0xff;
    let mut input = corrupt.to_vec();
    input.extend_from_slice(&SPEC_POLL);
    assert_eq!(serve_once(&input).await, ZERO_AMP_RESPONSE);
}

#[tokio::test]
async fn connects_out_to_gateway_and_answers_poll() {
    // Fake gateway: a loopback TCP listener our service dials out to (SPECS §3).
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let serve = tokio::spawn(connect_and_serve(addr, PollMatch::default(), zero_amps));

    let (mut gateway, _) = listener.accept().await.unwrap();
    gateway.write_all(&SPEC_POLL).await.unwrap();
    let mut response = [0u8; 17];
    gateway.read_exact(&mut response).await.unwrap();
    assert_eq!(response, ZERO_AMP_RESPONSE);

    serve.abort();
}
