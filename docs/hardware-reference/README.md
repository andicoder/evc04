# Vestel EVC04 — hardware reference

Vendor documentation and reverse-engineering notes for the **Vestel EVC04-AC11-T2P**
wallbox (basic Home variant, no comms module) — the hardware this repo's firmware
runs inside. It backs both paths: the RS485 meter emulation (control) and the CN28
LOG telemetry tap. Start at [`../overview.md`](../overview.md); the CN28 protocol is
[`../cn28-log-protocol.md`](../cn28-log-protocol.md).

Background issues (in the infra repo that used to host the retired k3s daemon):
[andicoder/[private-repo]#83](https://github.com/andicoder/[private-repo]/issues/83)
(meter-emulation control path),
[#87](https://github.com/andicoder/[private-repo]/issues/87) (internal-header
research), [#104](https://github.com/andicoder/[private-repo]/issues/104) (plug /
charge-power detection).

## Files here

| File | What it is |
|---|---|
| `EVC04_SERVICE_MANUAL_2021.pdf` | Official EVC04 AC22-AC7.4 Service Manual (2021). Board layout, connector legend, software-update via Veslink, **Ch. 8 log retrieval via the LOG socket**, error-code table. |
| `EVC04_SERVICE_MANUAL_2021.txt` | `pdftotext -layout` extraction (grep-friendly). |
| `EVC04_Modbus_RTU_Specification_v2_2021.pdf` | Official Modbus RTU register map (box-as-slave). `5004` = charging current R/W, `1020` active power, `1001` charging state, etc. Gated behind the comms module / Smart variant. |
| `board-layout-figure-2.1.png` | Service-manual Figure 2.1 (main board top view) with the numbered callouts. |
| `board-layout-LOG-socket-annotated.png` | Same figure, LOG socket (item 7) ringed, JTAG/DEBUG (item 8) boxed for contrast. |
| `board-photo-real-AC11.jpeg` | Photo of **our actual AC11 mainboard** near the rotary (silkscreen readable). |
| `board-photo-real-CN28-LOG-CN25-VESLINK.jpeg` | Zoom: `CN28 LOG` (4-pin), `CN25 VESLINK` (~10-pin), SW2 rotary, DIP. |
| `IMG_7188.jpg` | Macro of the real AC11 silkscreen at the **meter bus**: `CN24` (`5V`/`GND`) + `CN20` (`A`/`B`) RS485 data, plus `CN1/EN2` — confirms the connector map (the Waveshare's white/orange leads land on CN20 `A`/`B`). |
| `fig7-7_*`, `fig8-2_*` | Service-manual figures: Veslink/CN25 flash cable, EVC-Tester at the LOG socket. |

Sources (in case re-download is needed):
- Service manual: `https://s3fs-sogedis.s3.eu-west-3.amazonaws.com/sogedis_pdf/codespanne/EVC04_SERVICE_MANUAL_2021.pdf`
- Modbus spec: `https://api.library.loxone.com/downloader/file/722/EVC04_Modbus_RTU_Specification_v2_12.04.2021.pdf`
- Our board photos live in issue [#87](https://github.com/andicoder/[private-repo]/issues/87) (comment with 7 images).

## Mainboard connector legend (Service Manual Table 2-1)

> ⚠️ Documented for the AC22/AC7.4 board. Our AC11 board is the same generation but
> the layout near the rotary differs from these figures — **confirm by silkscreen on
> the real board**, don't trust pixel positions.

| # | Socket | # | Socket |
|---|---|---|---|
| 1-4 | Relay input terminals L3/L2/L1/N | 16 | HMI connection |
| 5 | Interlock (our silkscreen: **CN26**) | 17 | Rotary switch (SW2, hex current limit) |
| 6 | **Veslink** SW-update (our silkscreen: **CN25**) | 18 | DIP switch |
| 7 | **LOG connection socket** | 19 | LED socket |
| 8 | **J-TAG / DEBUG socket** (2-row pin header) | 20 | Power input |
| 9 | **CP/PP** (control pilot / proximity) | 21/22/30 | Protective earth |
| 10 | RCD 6 mA sensor | 23-26 | Relay output terminals |
| 11, 12 | **RS-485 socket ×2** (one = CN20 `5V GND A B` Power-Optimizer master bus) | 27/28/29 | **Current Transformer ×3** (internal MID metering) |
| 13 | Switch contact output | 14 | Enable/Input (our silkscreen: **CN1/EN2**) |
| 15 | RFID (our silkscreen: **CN5**) | | |

## LOG socket location (item 7) — the read-only telemetry lead

**Confirmed on the real AC11 board** (`board-photo-real-*.jpeg`): the LOG socket is
silkscreened **`CN28 LOG`** — a **4-pin** beige JST (best assessment **JST PH, 2.0 mm
pitch**, housing PHR-4; 🤔 confirm pitch with a caliper). It sits left/above the SW2
hex rotary; the **`CN25 VESLINK`** flash port (~10-pin) is *below* the rotary, and
**`CN20`** (the `5V GND A B` meter bus already wired to the Waveshare) is the RS485
connector.

In the (different-layout) AC22 service-manual Figure 2.1 the same socket is callout
**item 7** (`board-layout-LOG-socket-annotated.png`) — handy for the legend, but trust
the real-board silkscreen above for position.

The LOG stream (read today via Vestel's "EVC Tester" + the **Vestel EVC Configurator**
Android app over USB-OTG) reports live IEC-61851 pilot state with an `S:` prefix —
`A1/A2` unplugged, `B1/B2` plugged, `C1/C2` charging — plus an `ERROR:` code table.
This is a native, read-only source for **plug + charge state** (what #104 needs) that
the meter-emulation path structurally cannot provide.

Still 🤔: pin **order** (which of the 4 is VCC/TX/RX/GND), voltage level, and baud (the
EVC Tester is proprietary; the documented 115200 8N2 console is the *Smart-model* Linux
HMI board, not this MCU). Resolve by multimeter (GND = continuity to 0 V, TX idles high
~3.3 V) + baud sweep. **Read-only tap:** GND → FTDI GND, TX → FTDI RX, FTDI at 3.3 V.

## Key facts

- **LOG = `CN28`**, 4-pin beige JST (~JST PH 2.0 mm / PHR-4). Mating pigtail: a 4-pin
  JST PH 2.0 lead; for the read-only probe only GND + TX are used into an FTDI/CP2102.
- **VESLINK (CN25) = firmware-flash only**, via the WG-VESTA Veslink programmer +
  "Cable 7". Separate from the runtime LOG socket and the JTAG/DEBUG header.
- **Two RS-485 sockets** exist (items 11/12). CN20 is the Power-Optimizer master bus
  (already driven by `evc04-charge`); the second is a lead for an internal MID/HMI
  Modbus link.
- **No slave Modbus at the basic-model terminals** — the documented control registers
  (`5004` etc.) need the Smart/Connect variant or comms module. Hence meter emulation.
- The box **measures its own per-phase current internally** (3× CT → MID); that data
  is just not exposed without the comms module.
