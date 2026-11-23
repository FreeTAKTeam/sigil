# BSS radio compatibility for R3AKT SIGIL

**Reviewed:** 2026-11-23  
**Scope:** Analog FM voice + proprietary BSS packet signaling received over the air by SIGIL's RTL-SDR gateway. This is a **candidate compatibility list**, not a claim that all listed radios have passed SIGIL integration tests.

SIGIL currently decodes Bell 202 AFSK at 1,200 baud, NRZI/HDLC and CRC-16/X.25, then parses BSS sender ID and optional position information. The [SIGIL decoder](../src/bss.rs) and [developer guide](developer-guide.md) are the source of truth for supported BSS fields. Correct voice attribution additionally requires an appropriate BSS ID transmission on the same radio channel, commonly after PTT release.

## Compatibility status legend

- **Verified in SIGIL:** On-air reception and sender/position decoding demonstrated with the SIGIL gateway.
- **Documented BSS — untested in SIGIL:** Vendor documentation explicitly mentions BSS; expected to work, but exact frame format, firmware, GPS fields and trailing-ID timing still need RF testing.
- **Partial/conditional:** BSS-like or documented BSS capability with dependencies (such as phone-supplied GPS); some SIGIL features may be unavailable.
- **Unconfirmed:** Shared platform/app or third-party reports suggest BSS but independent model-level RF evidence is insufficient. Do not advertise as SIGIL-compatible yet.

## Radios with documented BSS signaling

| Model | Form factor | Advertised RF power | BSS ID | GPS / position source | SIGIL integration status | Source |
|---|---|---:|---|---|---|---|
| **Vero / VGC VR-N76** | Handheld | 5 W | Yes | Built-in GPS | **Verified** — current reference, including same-channel voice/BSS and position decoding | [Vendor](https://www.verotelecom.com/products/VGC-VR-N76-Dual-Band-Ham-Radio-KISS-TNC-Bluetooth-APRS-Satellite-Tracking-p2511333.html), [SIGIL test procedure](../README.md#receive-bss-packets) |
| Vero / VGC VR-N7600 | Mobile | 50 W | Documented | Built-in GPS | Documented BSS — **not tested** | [Vendor](https://www.verotelecom.com/Products/VERO-VR-N7600-50W-Dual-Band-Mobile-Radio-With-APP-Programming-p2581203.html) |
| BTECH UV-PRO | Handheld | Up to 7 W (variant dependent) | Documented | Built-in GPS | Documented BSS — **not tested** | [Manufacturer manual (PDF)](https://baofengtech.com/wp-content/uploads/2024/08/BTECHpackaging_UVPRO_Manual_Rev3.pdf), [firmware notes](https://baofengtech.com/uv-pro-firmware-changelog/) |
| BTECH UV-50PRO | Mobile | 50 W | Documented | Built-in GPS | Documented BSS — **not tested** | [Vendor](https://baofengtech.com/product/uv-50pro/), [firmware notes](https://baofengtech.com/uv-50pro-firmware-changelog/) |
| Radioddity GA-5WB | Handheld | 5 W | Documented | Built-in GPS | Documented BSS — **not tested** | [Manufacturer manual (PDF)](https://radioddity.s3.amazonaws.com/Radioddity_GA-5WB_Manual_20240409.pdf), [vendor](https://www.radioddity.com/products/radioddity-ga-5wb) |
| Radioddity DB50-B | Mobile | 50 W | Documented | Built-in GPS | Documented BSS — **not tested** | [Vendor extended manual](https://www.radioddity.com/products/extended-manual-for-radioddity-db50-b) |
| Radtel RT-660 | Handheld | Up to 7 W | Documented | Built-in GPS | Documented BSS — **not tested** | [Manufacturer listing](https://www.radtels.com/products/radtel-rt-660-ip67-dual-band-vhf-uhf-two-way-radio-gps-aprs-bluetooth-satcom-kiss-tnc-text-messaging-aviation-band-scanner-noaa-weather-alerts-usb-c-rechargeable-battery?variant=44702183260368) |
| Radtel RM-06 | Mobile | 50 W VHF / 40 W UHF | Documented | Built-in GPS | Documented BSS — **not tested** | [Manufacturer listing](https://www.radtels.com/products/radtel-rm-06-50w-dual-band-mobile-radio-vhf-uhf-amateur-ham-transceiver-with-gps-aprs-bluetooth-app-programming-ai-noise-reduction-satellite-tracking) |
| BTECH GMRS-50PRO | Mobile (US GMRS) | 50 W | Documented | Built-in GPS | Documented BSS — **not tested**; spectrum/equipment authorization is jurisdiction-dependent | [Manufacturer listing](https://baofengtech.com/product/gmrs-50pro/) |
| Vero / VGC VR-N7500 | Headless mobile | 50 W | Documented | GPS may depend on paired phone / accessory | **Partial/conditional** — sender signaling plausible; independently verify position and PTT timing | [Operating manual](https://device.report/manual/10316823), [HTCommander radio list](https://ylianst.github.io/HTCommanderSite/radios.html) |

**Note:** A manufacturer's BSS feature does **not** establish that its firmware transmits the precise frame variant supported by SIGIL, that BSS is sent after PTT, or that latitude/longitude are transmitted independently of a connected app. These require capture tests.

## Other BSS-family candidates needing confirmation

| Model | Form factor | Available evidence | Current SIGIL classification |
|---|---|---|---|
| BTECH GMRS-PRO | Handheld (US GMRS) | GPS, RF text/location signaling and shared BTECH app family; precise emitted BSS frame compatibility not yet established here | **Unconfirmed / conditional** |
| Vero / VGC VR-N75 | Handheld | Bluetooth app and position features; appears in the HTCommander Benshi-family radio list | **Unconfirmed / conditional** |
| Retevis RT78 | Handheld | Shared control/software family reported by third parties; no verified BSS RF capture | **Unconfirmed** |
| Retevis RT99 | Mobile | Shared control/software family reported by third parties; no verified BSS RF capture | **Unconfirmed** |

Sources: [BTECH GMRS-PRO](https://baofengtech.com/product/gmrs-pro/), [VR-N75](https://www.verotelecom.com/Products/VERO-App-Programming-Two-Way-Radio-VR-N75-p1623587.html), [HTCommander supported radios](https://ylianst.github.io/HTCommanderSite/radios.html), [Retevis app support](https://www.retevis.com/pages/app-download). Shared Bluetooth programming compatibility is **not** proof of over-the-air BSS compatibility.

## SIGIL operating modes

| Mode | Required on-air capability | Expected SIGIL output |
|---|---|---|
| Full voice + BSS | Analog FM voice; CRC-valid BSS sender and supported GPS position; verified PTT timing | Transcribed voice attributed to registered sender; position/telemetry in RCH |
| ID-only BSS | Analog FM voice; CRC-valid BSS sender, no valid GPS fix | Attributed voice/chat; no current-location telemetry |
| BSS location-only | Valid BSS location packet without voice | Radio position event (when sender identity is available); no transcript |
| Analog voice only | Analog FM, no BSS | Transcription/Kodiak interpretation; sender unresolved or explicitly labeled as gateway, never inferred as a specific radio |

Standard **APRS/AX.25 is not BSS**. SIGIL APRS reception is separately tracked in [issue #6](https://github.com/FreeTAKTeam/sigil/issues/6).

## RF acceptance checklist for a new BSS radio

1. Record model, firmware version, app version, channel frequency, analog FM bandwidth, BSS/APRS format selection, GPS source and BSS ID settings.
2. Capture raw discriminator WAV with `--bss-capture` and diagnostics enabled; preserve an independent record of displayed GPS position.
3. Verify CRC-valid BSS sender packet and, where claimed, latitude/longitude, speed, altitude and heading decoding; record unknown fields without guessing their meaning.
4. Transmit voice followed by BSS ID/position. Confirm timing, correlation, and attribution across at least 10 transmissions, including back-to-back speakers and intervening periodic beacons.
5. Confirm that `--bss-only` works without ASR; confirm combined voice+BSS does not pass AFSK noise to the speech recognizer.
6. Confirm RCH/LXMF chat and position delivery for an allowlisted identity; unknown IDs must not acquire privileges.
7. Add raw capture fixtures and regression tests before upgrading the model to **Verified in SIGIL**.

**Testing priority:** First test a 50 W mobile (VR-N7600 or RM-06) and one different branded handheld (UV-PRO or GA-5WB). Prefer an independently implemented radio protocol for later interoperability tests.

**Not covered:** RF licence/certification status, market availability or current prices. Suitability for transmission in Canada must be checked separately.
