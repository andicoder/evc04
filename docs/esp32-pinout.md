# ESP32 pinout — CN28 read + RS485 meter emulation

Pin assignment for the **single-MCU in-box bridge** (SPECS §7, #65): one classic
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
| UART1 | CN28 LOG read (9600 8N1)      | GPIO16 TX → Box-RX, GPIO17 RX ← Box-TX |
| UART2 | RS485 meter emulation (9600 8E1) | GPIO25 TX, GPIO26 RX (no DE — auto-direction) |

UART0 stays reserved for the USB monitor and the very first flash; once sealed,
updates go over OTA (#76), but the console pins are never repurposed. UART1 is the
existing CN28 tap. UART2 is the new RS485 side and the subject of this doc.

## CN28 read — how the box side is wired

The CN28 LOG console is a plain **3.3 V TTL UART** (9600 8N1), so it wires **direct
to the ESP — no transceiver, no level shifter, no DE line**. This side is unchanged
by the move to the TTL485 v2; it is spelled out here so the whole box is in one
place.

**Connector: 4-pin header, counted from the bottom** (go by pin position — the CN28
silk labels are unreliable):

| Pin (from bottom) | Signal              | Wires to ESP32     |
|-------------------|---------------------|--------------------|
| 1                 | GND                 | GND                |
| 2                 | box TX (box sends)  | GPIO17 (UART1 RX)  |
| 3                 | box RX (box recvs)  | GPIO16 (UART1 TX)  |
| 4                 | — (NC)              | leave unconnected  |

```
ESP32 GPIO17 (UART1 RX) ◄────────── CN28 pin 2   box TX → ESP RX
ESP32 GPIO16 (UART1 TX) ──────────► CN28 pin 3   ESP TX → box RX
ESP32 GND              ─────────────  CN28 pin 1  (GND)
```

- Normal UART **cross-over**: box transmit (pin 2) → ESP **RX** (GPIO17); box receive
  (pin 3) ← ESP **TX** (GPIO16) — never RX-to-RX (see
  [`cn28-log-protocol.md`](cn28-log-protocol.md)).
- **Common GND** between the ESP and CN28 — required for a single-ended TTL link.
- **Pin 4 (NC)** has no supply; the ESP is powered separately over USB.
- 9600 **8N1** (no parity — the RS485 side below is 8E1; different parity, both at
  9600, independent UART controllers).

## RS485 (Modbus) — recommended pins

The box is **Master** on CN20 and polls the meter; we are the **slave** the box
reads (SPECS §2/§4). RS485 is a half-duplex differential bus, so it needs a
transceiver. We use a **TTL485 v2 auto-direction module**: it senses TX activity and
flips the bus direction itself, so there is **no DE line** to drive and **GPIO27 is
now free**.

```
ESP32  GPIO25 (UART2 TX) ──► RXD   ┐
ESP32  GPIO26 (UART2 RX) ◄── TXD   │ TTL485 v2       A ──► CN20  A
ESP32  3V3              ───  VCC    │ (auto-dir)      B ──► CN20  B
ESP32  GND             ───  GND    ┘                 GND ─ CN20 GND
```

- **GPIO25 (TX) → module `RXD`** (the module's receive input — our transmit).
- **GPIO26 (RX) ← module `TXD`** (the module's transmit output — the box's reply).
- **VCC → 3V3** (see the power warning below), **GND → GND**.
- **A/B → CN20 A/B**, plus a common **GND** to CN20 GND. Silkscreen on CN20 is
  `V | GND | A | B` (SPECS §2); leave the `V` (supply) pin alone — the ESP is
  powered separately.

Two module-specific gotchas that cause a silent no-comms bring-up:

- ⚠️ **Power the module from 3V3, not 5V.** On an auto-direction module the idle
  level of `TXD` follows VCC, and ESP32 GPIOs are **not 5 V tolerant** — a 5 V-fed
  module would idle GPIO26 at 5 V and can damage the pin. At 3V3 the line is safe,
  and auto-direction switches cleanly at 9600 baud. (If a specific module needs 5 V
  to switch reliably, add a divider on `TXD → GPIO26`.)
- 🤔 **`TXD`/`RXD` labelling is ambiguous** on these cheap boards. Wired as above
  (module `RXD` = its input, from the ESP TX). If there is no traffic, first swap
  `TXD`↔`RXD` at the module, then try swapping **A↔B** on the CN20 side.

### Why these pins

GPIO25/26 are regular digital I/O with **none** of the ESP32 gotchas:

- **not** strapping pins (GPIO0, 2, 5, 12, 15 — avoid: their boot level matters),
- **not** the SPI-flash pins (GPIO6–11 — never usable on a WROOM module),
- **not** input-only (GPIO34–39 cannot drive an output, so they are unusable for TX),
- **not** already taken by UART0 (GPIO1/3) or the CN28 UART1 (GPIO16/17).

They also sit together on one side of the DevKitC header, so the module wires to
adjacent pins. 🤔 Exact physical header position varies by board revision — go by the
**GPIO number printed on the board**, not by counting pins.

(GPIO25/26 double as the chip's DAC and ADC2 channels, but we only use them as
digital UART here, so that is irrelevant — and ADC2 is unusable with WiFi on
anyway.)

### Half-duplex turnaround

With the auto-direction module the firmware does **nothing** for direction control:
it enables the bus driver when we clock bytes out on `RXD` and returns to receive
after the line idles, all in hardware. `firmware/src/rs485.rs` just reads the poll
and writes the reply — no DE toggle, no TX-drain wait. (The earlier bare-MAX3485
design drove DE manually on GPIO27; that pin is unused now.)

## Bus parameters (must match the box)

These are fixed by the EVC04 / Inepro bus and are **not** negotiable (SPECS §3/§4):

```
RS485:  9600 baud, 8 data bits, EVEN parity, 1 stop bit (9600 8E1)
Slave:  address 1, FC 0x03, start 0x500C, qty 6, polled ~1.006 s
```

Note the parity differs from the CN28 side (9600 **8N1**): both UARTs run at 9600
but with different parity (8N1 vs 8E1), which is fine since they are independent
controllers.

## Pins to avoid

| Pins              | Why off-limits                                              |
|-------------------|------------------------------------------------------------|
| GPIO6–11          | wired to the WROOM SPI flash — using them crashes the chip  |
| GPIO0, 2, 5, 12, 15 | strapping pins — their level at boot selects boot mode/flash voltage |
| GPIO34–39         | input-only — no output driver, so no TX                     |
| GPIO1, 3          | UART0 USB console / first-flash path (SPECS §7)             |
| GPIO16, 17        | UART1 CN28 read (SPECS §7)                                  |

## Full device wiring at a glance

```
                ┌──────────────── ESP32 DevKitC V4 ────────────────┐
   USB / log ───┤ GPIO1/3  (UART0)                                  │
                │                                                   │
   CN28 LOG  ───┤ GPIO16 → Box-RX   GPIO17 ← Box-TX   GND  (UART1)  │
   (3.3V TTL,   │   9600 8N1, wire direct — no level shifter        │
    read)       │                                                   │
                │ GPIO25 → RXD                                      │
   CN20 RS485 ──┤ GPIO26 ← TXD  ──►  TTL485 v2 ──►  A / B / GND     │
   (meter       │   (no DE)          auto-dir       to CN20         │
    emulation)  │   9600 8E1, slave addr 1                          │
                └───────────────────────────────────────────────────┘
```

See [`SPECS.md`](SPECS.md) for the wire protocol, register
map, and control math, and [`overview.md`](overview.md) for how the read and
control paths fit together.
