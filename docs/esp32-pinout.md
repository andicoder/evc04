# ESP32 pinout — CN28 read + RS485 meter emulation

Pin assignment for the **single-MCU in-box bridge** (SPECS §12, #65): one classic
ESP32 (AZ-Delivery **DevKit C V4**, 38-pin WROOM-32) doing **both** jobs —

- **read** the CN28 LOG console over a TTL UART (already shipping, #66/#70), and
- **control** by emulating the Inepro PRO380 meter on the box's RS485 bus (CN20),
  which is what replaces the Waveshare gateway + the k3s `charge` daemon.

This is the wiring reference for that second port. The CN28 side is unchanged; it
is repeated here so the whole device is described in one place.

## The three UARTs

The ESP32 has three hardware UART controllers and a full GPIO matrix, so any UART
can be routed to almost any pin (the silkscreen `TX2`/`RX2` labels are only the
*defaults*). We use all three:

| UART  | Role                          | Pins (this design)                 |
|-------|-------------------------------|------------------------------------|
| UART0 | USB log console / first flash | GPIO1 / GPIO3 — **leave free**     |
| UART1 | CN28 LOG read (115200 8N1)    | GPIO17 TX → Box-RX, GPIO16 RX ← Box-TX |
| UART2 | RS485 meter emulation (9600 8E1) | GPIO25 TX, GPIO26 RX, GPIO27 DE |

UART0 stays reserved for the USB monitor and the very first flash; once sealed,
updates go over OTA (#76), but the console pins are never repurposed. UART1 is the
existing CN28 tap. UART2 is the new RS485 side and the subject of this doc.

## RS485 (Modbus) — recommended pins

The box is **Master** on CN20 and polls the meter; we are the **slave** the box
reads (SPECS §2/§4). RS485 is a half-duplex differential bus, so it needs a
transceiver (MAX485 / MAX3485 / SP3485 — a 3.3 V part such as **MAX3485** matches
the ESP directly; the classic 5 V MAX485 needs its logic lines kept at 3.3 V).

```
ESP32  GPIO25 (UART2 TX) ──► DI    ┐
ESP32  GPIO26 (UART2 RX) ◄── RO    │  MAX3485
ESP32  GPIO27 (DE)       ──► DE+/RE┘  transceiver      A ──► CN20  A
ESP32  3V3              ───  VCC                        B ──► CN20  B
ESP32  GND             ───  GND ───────────────────────GND ─ CN20 GND
```

- **GPIO25 → DI** (transceiver data-in = UART2 TX)
- **GPIO26 ← RO** (transceiver data-out = UART2 RX)
- **GPIO27 → DE + /RE tied together** (direction control; high = transmit, low =
  receive). Tie the active-high `DE` and active-low `/RE` to the **same** GPIO so a
  single line flips the bus direction.
- **A/B → CN20 A/B**, plus a common **GND** to CN20 GND. Silkscreen on CN20 is
  `V | GND | A | B` (SPECS §2); leave the `V` (supply) pin alone — the ESP is
  powered separately.

### Why these pins

GPIO25/26/27 are regular digital I/O with **none** of the ESP32 gotchas:

- **not** strapping pins (GPIO0, 2, 5, 12, 15 — avoid: their boot level matters),
- **not** the SPI-flash pins (GPIO6–11 — never usable on a WROOM module),
- **not** input-only (GPIO34–39 cannot drive an output, so they are unusable for
  TX or DE),
- **not** already taken by UART0 (GPIO1/3) or the CN28 UART1 (GPIO16/17).

They also sit together on one side of the DevKitC header, so the transceiver wires
to three adjacent pins. 🤔 Exact physical header position varies by board revision —
go by the **GPIO number printed on the board**, not by counting pins.

(GPIO25/26 double as the chip's DAC and ADC2 channels, but we only use them as
digital UART here, so that is irrelevant — and ADC2 is unusable with WiFi on
anyway.)

### Driving the DE line

The DE direction line must be **high for the entire transmitted frame** and drop
back to receive **only after the last stop bit has left the wire** — releasing it
early truncates the final byte and the box sees a CRC error.

Two ways to get that right:

1. **Hardware-driven DE (preferred).** ESP-IDF's UART driver can manage the DE
   line itself via the RTS signal in `UART_MODE_RS485_HALF_DUPLEX`, so the
   hardware raises/lowers DE around each frame with exact timing — no software
   race. Route DE to the UART2 RTS pin (GPIO27 here). 🤔 Confirm `esp-idf-hal`'s
   `UartDriver` exposes this mode on the resolved crate version during the first
   real build; the API is version-sensitive (same caveat as the CN28 UART in
   `firmware/src/main.rs`).
2. **Manual GPIO toggle (fallback, always works).** Set GPIO27 high, write the
   frame, **wait for TX to fully drain** (flush / TX-done), then set it low. The
   wait is the part that's easy to get wrong — don't drop DE right after `write()`
   returns, that only means the bytes are queued, not sent.

Prefer option 1 if the HAL supports it cleanly; otherwise option 2 with an
explicit TX-complete wait.

## Bus parameters (must match the box)

These are fixed by the EVC04 / Inepro bus and are **not** negotiable (SPECS §3/§4):

```
RS485:  9600 baud, 8 data bits, EVEN parity, 1 stop bit (9600 8E1)
Slave:  address 1, FC 0x03, start 0x500C, qty 6, polled ~1.006 s
```

Note the parity differs from the CN28 side (115200 **8N1**) — the two UARTs run
different framing, which is fine since they are independent controllers.

## Pins to avoid

| Pins              | Why off-limits                                              |
|-------------------|------------------------------------------------------------|
| GPIO6–11          | wired to the WROOM SPI flash — using them crashes the chip  |
| GPIO0, 2, 5, 12, 15 | strapping pins — their level at boot selects boot mode/flash voltage |
| GPIO34–39         | input-only — no output driver, so no TX and no DE          |
| GPIO1, 3          | UART0 USB console / first-flash path (SPECS §12)            |
| GPIO16, 17        | UART1 CN28 read (SPECS §12)                                 |

## Full device wiring at a glance

```
                ┌──────────────── ESP32 DevKitC V4 ────────────────┐
   USB / log ───┤ GPIO1/3  (UART0)                                  │
                │                                                   │
   CN28 LOG  ───┤ GPIO17 → Box-RX   GPIO16 ← Box-TX   GND  (UART1)  │
   (3.3V TTL,   │   115200 8N1, wire direct — no level shifter      │
    read)       │                                                   │
                │ GPIO25 → DI                                       │
   CN20 RS485 ──┤ GPIO26 ← RO   ──►  MAX3485  ──►  A / B / GND      │
   (meter       │ GPIO27 → DE/RE     transceiver    to CN20         │
    emulation)  │   9600 8E1, slave addr 1                          │
                └───────────────────────────────────────────────────┘
```

See [`../charge/SPECS.md`](../charge/SPECS.md) for the wire protocol, register
map, and control math, and [`overview.md`](overview.md) for how the read and
control paths fit together.
