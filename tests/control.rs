//! Integration tests for the control seam (SPECS.md §6): the MQTT target current
//! drives the household current the meter slave serves.

use evc04_charge::control;
use evc04_charge::mqtt::TargetError;
use evc04_charge::slave::{serve_connection, PollMatch};
use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt};

const FUSE: f32 = 32.0;

// SPECS.md §4 verified poll frame.
const SPEC_POLL: [u8; 8] = [0x01, 0x03, 0x50, 0x0c, 0x00, 0x06, 0x14, 0xcb];

// SPECS.md §5 verified 0 A response (report 0 → box grants maximum charge).
const ZERO_AMP_RESPONSE: [u8; 17] = [
    0x01, 0x03, 0x0c, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x93, 0x70,
];

// SPECS.md §5 verified 16 A response (report 16 → 16 A headroom left for the car).
const SIXTEEN_AMP_RESPONSE: [u8; 17] = [
    0x01, 0x03, 0x0c, 0x41, 0x80, 0x00, 0x00, 0x41, 0x80, 0x00, 0x00, 0x41, 0x80, 0x00, 0x00, 0x97,
    0xae,
];

#[test]
fn target_zero_reports_full_fuse_per_phase() {
    // target = 0 → report the whole fuse → no headroom → charging pauses.
    let (_apply, currents) = control::channel(FUSE, 0.0);
    assert_eq!(currents(), [FUSE; 3]);
}

#[test]
fn target_at_fuse_limit_reports_zero() {
    // target = fuse_limit → report 0 → box grants maximum charge.
    let (apply, currents) = control::channel(FUSE, 0.0);
    apply(Ok(FUSE));
    assert_eq!(currents(), [0.0; 3]);
}

#[test]
fn rejected_target_holds_the_last_valid_value() {
    // A malformed command must not disturb the effective target (docs/mqtt.md).
    let (apply, currents) = control::channel(FUSE, 0.0);
    apply(Ok(10.0));
    apply(Err(TargetError::Malformed));
    assert_eq!(currents(), [22.0; 3]);
}

#[tokio::test]
async fn served_frame_tracks_the_live_target() {
    // The slave recomputes from the live target, so a new command is reflected in
    // the very next served poll without any extra signalling.
    let (apply, currents) = control::channel(FUSE, 16.0);
    let (mut client, server) = duplex(1024);
    let serve = tokio::spawn(serve_connection(
        server,
        PollMatch::default(),
        None,
        currents,
    ));

    // target 16 on a 32 A fuse → report 16 A.
    client.write_all(&SPEC_POLL).await.unwrap();
    let mut frame = [0u8; 17];
    client.read_exact(&mut frame).await.unwrap();
    assert_eq!(frame, SIXTEEN_AMP_RESPONSE);

    // Raise the target to the fuse limit → report 0 A on the next poll.
    apply(Ok(FUSE));
    client.write_all(&SPEC_POLL).await.unwrap();
    client.read_exact(&mut frame).await.unwrap();
    assert_eq!(frame, ZERO_AMP_RESPONSE);

    serve.abort();
}
