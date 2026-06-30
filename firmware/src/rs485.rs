//! RS485 meter-emulation slave (evc04#85): answer the EVC04 Power Optimizer's
//! meter poll directly on-device, so the box reads the spoofed Inepro PRO380 over
//! its CN20 bus with no Waveshare TCP↔RS485 gateway in the loop.
//!
//! The box is **master** and polls `addr 1 / FC03 / 0x500C × 6` at ~1.006 s
//! (SPECS §4); we are the slave. The framing/encoding is the host-tested
//! [`evc04_cn28_core::charge::frame`] logic the daemon proved on hardware — this module is
//! only the UART + half-duplex direction glue around it.
//!
//! Wiring (UART2 + MAX3485, see `docs/esp32-pinout.md`):
//!   GPIO25 (TX) → DI · GPIO26 (RX) ← RO · GPIO27 → DE/RE · A/B → CN20 · 9600 8E1
//!
//! Scope (#85): answer the poll with a static bench value ([`BENCH_REPORT_AMPERE`]).
//! MQTT-driven values are #86; coexistence with the CN28 read loop and a continuity
//! watchdog are #87.
//!
//! Compiles against the pinned esp-idf-hal 0.46.2 / esp-idf-svc 0.52.1. What the
//! build can't prove is the DE *timing* on the wire — that `wait_tx_done` holds DE
//! asserted until the last stop bit is out — so confirm that with a scope/logic
//! analyzer on the bench (#88).

use std::sync::{Arc, Mutex};
use std::time::Instant;

use esp_idf_svc::hal::delay::TickType;
use esp_idf_svc::hal::gpio::{Output, PinDriver};
use esp_idf_svc::hal::uart::UartDriver;
use esp_idf_svc::sys::{esp_err_t, ESP_ERR_TIMEOUT};
use evc04_cn28_core::charge::frame::{build_response, encode_currents, parse_request};
use log::{info, warn};

use crate::control::ControlState;

/// RS485 meter bus baud (CN20): the box polls the emulated PRO380 at 9600 8E1
/// (SPECS §3). `main` configures UART2 with it.
pub const BAUD: u32 = 9_600;

/// The meter poll we emulate (SPECS §4). Fixed: the box only ever issues this one.
const SLAVE_ADDR: u8 = 1;
const POLL_REGISTER: u16 = 0x500C;
const POLL_QUANTITY: u16 = 6;

/// Block this long waiting for the *first* byte of a poll before looping (the box
/// polls ~1 s, so this just bounds the idle wait — it is not a frame timeout).
const FIRST_BYTE_MS: u64 = 1000;
/// Inter-byte gap that marks the end of an RTU frame. Modbus delimits frames by
/// ≥3.5 character times of silence; at 9600 8E1 a char is ~1.15 ms, so 3.5 ≈ 4 ms.
/// 20 ms is comfortably above that (and above the FreeRTOS tick) without risking a
/// merge with the next ~1 s poll.
const READ_GAP_MS: u64 = 20;
/// Cap on the wait for the TX FIFO to drain before releasing DE — a full 8N/8E
/// frame is tens of bytes at most, milliseconds on the wire.
const TX_DRAIN_MS: u64 = 100;
/// An RTU read request is 8 bytes; a little headroom absorbs line noise.
const READ_BUF: usize = 32;

/// Thread routine: serve the meter-emulation slave forever — assemble each inbound
/// poll, and when it is *our* poll, answer with [`BENCH_REPORT_AMPERE`]. `main`
/// owns construction of `uart`/`de` and moves them onto this thread (#86 will feed
/// the reported value from MQTT instead of the bench const).
pub fn run(
    uart: UartDriver<'static>,
    mut de: PinDriver<'static, Output>,
    control: Arc<Mutex<ControlState>>,
) {
    // Receive is the default line state; only flip DE high around our own transmit.
    let _ = de.set_low();
    info!("rs485: meter slave up (addr {SLAVE_ADDR}, 0x{POLL_REGISTER:04x}×{POLL_QUANTITY}, 9600 8E1)");

    let first = TickType::new_millis(FIRST_BYTE_MS).ticks();
    let gap = TickType::new_millis(READ_GAP_MS).ticks();
    let mut chunk = [0u8; READ_BUF];

    loop {
        // Assemble one frame: wait (idle) for the first byte, then drain until the
        // line goes quiet for READ_GAP_MS — the RTU end-of-frame marker.
        let mut frame: Vec<u8> = Vec::with_capacity(READ_BUF);
        loop {
            let timeout = if frame.is_empty() { first } else { gap };
            match uart.read(&mut chunk, timeout) {
                Ok(0) => break,
                Ok(n) => frame.extend_from_slice(&chunk[..n]),
                // A read timeout means the line went quiet — the frame is complete
                // (or nothing came), not an error. (Same idiom as the prober.)
                Err(e) if e.code() == ESP_ERR_TIMEOUT as esp_err_t => break,
                Err(e) => {
                    warn!("rs485: uart read error: {e}");
                    break;
                }
            }
        }
        if frame.is_empty() {
            continue; // idle poll gap, nothing to answer
        }

        // Validate CRC + recognise our exact poll; anything else is dropped silently
        // (Modbus "silence on foreign address"). The cadence is content-agnostic, so
        // a dropped poll just means the box re-polls in ~1 s.
        let Some(req) = parse_request(&frame) else {
            continue;
        };
        if !req.is_poll(SLAVE_ADDR, POLL_REGISTER, POLL_QUANTITY) {
            continue;
        }

        // Our poll: stamp it (liveness) and serve the latest control-loop value
        // (#86) on all three phases. The lock is held only for these two reads.
        let amps = {
            let mut state = control.lock().unwrap();
            state.note_poll(Instant::now());
            state.reported()
        };
        let payload = encode_currents(amps, amps, amps);
        let response = build_response(SLAVE_ADDR, &payload);

        // Half-duplex turnaround: assert DE, transmit, and hold DE until the last
        // stop bit has left the wire. Dropping DE right after `write` returns only
        // means the bytes are queued, not sent — releasing early truncates the final
        // byte and the box sees a CRC error (see docs/esp32-pinout.md "Driving DE").
        // (The hardware RS485-via-RTS alternative would replace this block; the
        // manual path is used here because it is correct regardless of HAL support.)
        let _ = de.set_high();
        if let Err(e) = uart.write(&response) {
            warn!("rs485: uart write error: {e}");
        }
        // `wait_tx_done` blocks until the TX FIFO is empty. Whether that is late
        // enough to hold DE through the final stop bit is the one thing only a bench
        // scope can confirm (#88); if it releases early, add a short post-delay.
        let _ = uart.wait_tx_done(TickType::new_millis(TX_DRAIN_MS).ticks());
        let _ = de.set_low();
    }
}
