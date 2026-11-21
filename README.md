<p align="center">
  <img src="docs/images/sigil-logo.webp" alt="R3AKT Sigil logo: a radio tower inside a glowing ring" width="320">
</p>

# R3AKT Sigil

R3AKT Sigil receives analog narrowband FM on an RTL-SDR. It transcribes speech locally, decodes BSS radio IDs and position reports, and can publish to RCH from a persistent LXMF identity for each authorized radio. The default channel is 435.9950 MHz.

## Run the embedded gateway

Copy `gateway.example.toml` to `gateway.toml`. Set `[identity].allowlist` to the radios you operate, `[fabric].tcp_clients` to your Reticulum TCP interfaces, and `[rch].destination_hash` to the hub’s LXMF delivery hash. Set `[rch].enabled = true` after checking the recipient. `[identity].auto_register` permits the first registration of allowlisted radios.

```powershell
$env:R3AKT_IDENTITY_PASSPHRASE = Read-Host 'Identity registry passphrase' -MaskInput
.\target\release\r3akt-sigil.exe listen --config .\gateway.toml --backend parakeet --gain-db 0 --save-wav
```

The passphrase must contain at least 16 bytes. Use a strong unique passphrase and retain it separately from backups. On Linux, provide the same environment variable and use the `whisper` backend in the default build. Identity and Fabric paths are relative to the TOML file. A gateway configured without RCH still writes correlated events locally. `--bss-only` listens for IDs and locations without loading an ASR model.

Configure the radio to send BSS ID or location on PTT release. Wait for `Listening on 435.9950 MHz` before transmitting. `gateway_*.json` files identify correlated or unresolved voice and independently decoded positions. The managed LXMF source appears in the Fabric journal under `data/fabric/<rch-delivery-hash>`. A transport proof confirms native Reticulum delivery. Verify application ingestion in RCH chat and on the telemetry map.

The [gateway architecture](docs/architecture.md) covers event correlation, recovery, telemetry compatibility, and identity backups. The [developer guide](docs/developer-guide.md) describes the modules, signal chain, threading model, file schemas and tests.

## Run on Windows

Run these commands from the project directory. Save the Whisper base.en model as `models/ggml-base.en.bin` or use `--model` to select another local whisper.cpp model. [Download base.en](https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin)

The release executable uses the x64 Visual C++ v14 runtime. [Microsoft Visual C++ Redistributable](https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist/)

```powershell
.\target\release\r3akt-sigil.exe listen --save-wav --monitor-power
```

Leave the channel idle until the console prints `Listening on 435.9950 MHz`. Each completed transmission goes to a separate inference worker so the receiver can keep capturing. Ctrl+C closes the receiver and drains queued clips. A continuous open carrier is split into clips up to 30 seconds.

JSON files in `transcripts` contain the source, frequency in Hz, UTC timestamps, audio duration and transcript text. An empty text field indicates Whisper returned no text. `--save-wav` adds a 16 kHz mono PCM file alongside each JSON. Live start and end timestamps track the received carrier. For WAV imports they record the import time.

### Tune and set gain or squelch

```powershell
.\target\release\r3akt-sigil.exe listen --frequency-hz 435995000 --gain-db 29.7 --squelch-dbfs -42 --monitor-power
```

Gain is rounded to the nearest step the tuner supports. By default the listener measures the idle channel for roughly one second and sets the squelch 8 dB above its median level. For manual squelch, use `--monitor-power` to observe idle and active channel levels, then set `--squelch-dbfs` between them. A more negative threshold opens on weaker signals. See `listen --help` for device index and oscillator correction.

### Check transcription from WAV

```powershell
.\target\release\r3akt-sigil.exe wav .\voice.wav --save-wav --output-dir .\wav-check
```

WAV mode accepts PCM or float input between 8 and 192 kHz, up to five minutes. It downmixes and resamples the file for transcription. The frequency is a label in WAV mode.

### Use Parakeet TDT v2 for English

Build with the `parakeet` feature and extract the Sherpa-ONNX INT8 English model to `models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8`. It contains `encoder.int8.onnx`, `decoder.int8.onnx`, `joiner.int8.onnx`, and `tokens.txt`. Use `--model` with a directory path to place it elsewhere. [Sherpa-ONNX model download](https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8.tar.bz2) · [NVIDIA Parakeet TDT v2](https://huggingface.co/nvidia/parakeet-tdt-0.6b-v2)

```powershell
cargo build --release --locked --features parakeet
.\target\release\r3akt-sigil.exe listen --backend parakeet --gain-db 16.6 --save-wav
.\target\release\r3akt-sigil.exe wav .\voice.wav --backend parakeet --output-dir .\wav-check
```

Keep the Sherpa-ONNX and ONNX Runtime DLLs next to the Parakeet-enabled Windows executable. The build places them in the release directory. Whisper remains the default backend for existing commands.

## Build and test

Use the pinned Rust MSVC toolchain, Visual Studio C++ Build Tools with a Windows SDK, CMake on PATH, and an x64 LLVM installation. Point `LIBCLANG_PATH` to the directory containing `libclang.dll` if Cargo cannot discover it. The headless build fetches the pinned SDR-RS DSP dependencies and compiles bundled libusb and whisper.cpp.

```powershell
cargo build --release --locked
cargo test --release --locked
```

Windows USB requires the dongle’s SDR interface to expose a working USB driver compatible with libusb. WinUSB is the supported setup for the NESDR. [Nooelec installation guide](https://support.nooelec.com/hc/en-us/articles/360005298053-NESDR-Installation-Guide) · [libusb Windows support](https://github.com/libusb/libusb/wiki/Windows)

The radio path uses the CPU-only [SDR-RS DSP crates](https://github.com/jasonherald/rtl-sdr/tree/4380abf4eb7e74953623d063477f89f9b47876d6/crates/sdr-dsp) for decimation, filtering, FM demodulation and channel squelch. The USB driver is [librtlsdr-rs](https://github.com/jasonherald/librtlsdr-rs). The transcription path uses the same [whisper-rs binding](https://docs.rs/whisper-rs/0.16.0/whisper_rs/) as SDR-RS.

### Package for Windows and Linux

```powershell
.\scripts\package-windows.ps1
.\scripts\package-windows.ps1 -WhisperOnly -ModelPath .\models\ggml-base.en.bin
```

The Windows package includes the native executable, runtime DLLs for its selected backend, configuration example, and instructions. From the extracted bundle, run `.\r3akt-sigil.exe` in place of `.\target\release\r3akt-sigil.exe`. Use `-ModelPath` to include a local compatible model. On Linux x64 or ARM64, install a C/C++ toolchain, CMake, Clang and libclang, then run `sh scripts/package-linux.sh`. You can pass a local Whisper model to include it. Linux requires USB access for the account running the gateway. The embedded mode runs within the executable.

## Live acceptance

With the Nooelec connected, start the listener with `--save-wav`. Check that it reports `Tuned RTL-SDR device 0 to 435.9950 MHz (435995000 Hz)`. Once it reports listening, use two known analog NFM voice transmissions on that channel with enough time for the squelch to close in between. Confirm two detection and completion cycles, two readable console transcripts, and two corresponding JSON and WAV pairs in `transcripts`. A WAV test exercises local inference and file output. The live test also exercises USB tuning, RF capture and demodulation.

## Legacy REST publisher

Use this existing standalone publisher with the legacy voice workflow. Stop the watcher before using embedded publishing on the same event directory. It requires Python 3.11 or later and a running Fabric or YARA HTTP service. Set `api_base_url` in `lxmf.toml` to the live bind address, and set `sender_name` and `destination` for the chat recipient.

To submit a standalone test and then start automatic publishing, run these commands from the project directory.

```powershell
python .\send_lxmf.py --text "R4 transmission test"
python .\send_lxmf.py --watch
```

In a second PowerShell window, run the receiver as usual.

```powershell
.\target\release\r3akt-sigil.exe listen --save-wav
```

The publisher watches `transcripts` for completed live JSON files. If the receiver uses `--output-dir`, give the publisher the same directory with `--watch-dir`. On the first watch of a directory, the publisher skips its existing files. Subsequent runs catch up on new files created while the publisher was stopped. The outbox records each live transcript once and ignores blank transcripts and WAV imports.

The message header uses the configured sender label, the actual captured frequency, and the transmission end time in the local timezone. A manual test uses `[radio].frequency_hz` and the current local time. The original speech text follows the header. The publisher sends text through the existing MeshChat-compatible route. [Fabric send handler](https://github.com/FreeTAKTeam/LXMF-rs/blob/008bf9e1cbc2b3f6859abf21f64dfdb87be8621f/crates/libs/meshchat-api/src/chat.rs)

### Submission and recovery

`Fabric accepted message` logs the returned LXMF hash. Check the destination chat to verify peer receipt. The receiver continues capturing while the publisher sends or retries.

The SQLite outbox is `.lxmf-outbox.sqlite3` beside the script. Keep it across restarts for deduplication. Connection failures before sending the POST use bounded backoff up to `retry_attempts` total attempts. After Fabric is reachable again, requeue safely deferred submissions with this command.

```powershell
python .\send_lxmf.py --retry-pending
```

The outbox retains HTTP rejections, uncertain responses, and interrupted in-flight attempts for review. An uncertain response can occur after Fabric accepts the message. Check Fabric's conversation history before submitting an uncertain message manually.

### Sender identity shown by the peer

`sender_name` controls the label in the message body. Fabric supplies the actual LXMF source identity. To show `R4` as the peer name, set the active YARA profile's display name to `R4` and announce that identity. For standalone Fabric, set `display_name = "R4"` at the top level of the daemon's active TOML, restart it with the same identity file, and announce. [YARA daemon launcher](https://github.com/FreeTAKTeam/YARA/blob/main/electron/rust-poc-launcher.js)

### Publisher tests

```powershell
python -m unittest -v test_send_lxmf
```

The automated tests exercise the standalone command and automatic publishing against a local HTTP fixture. They cover malformed destinations, Unicode, multiple transcripts, file deduplication across restarts, connection failures, and ambiguous responses. To verify live delivery, send several authorized NFM voice messages after both consoles are ready. Check one accepted hash per transcript, then confirm the messages in the recipient's chat.

## Receive BSS packets

Set the radio's Digital Channel to the intended frequency and select the BSS format. This VR-N76 is configured for 435.9950 MHz on both voice and data. One SDR receives one channel at a time. Use `--frequency-hz` to select the BSS channel, either with voice on that channel or in BSS-only mode.

```powershell
# Capture BSS, including a raw discriminator WAV and failed HDLC candidates.
.\target\release\r3akt-sigil.exe listen --bss-only --frequency-hz 435995000 --gain-db 16.6 --output-dir .\bss-check --bss-diagnostics --bss-capture .\vr-n76-discriminator.wav

# Replay the capture after closing the live receiver with Ctrl+C.
.\target\release\r3akt-sigil.exe bss-wav .\vr-n76-discriminator.wav --frequency-hz 435995000 --output-dir .\bss-replay --bss-diagnostics

# Receive voice and BSS on the same channel with a Parakeet-enabled build.
.\target\release\r3akt-sigil.exe listen --bss --backend parakeet --frequency-hz 435995000 --gain-db 16.6
```

`--bss-capture` writes continuous unfiltered mono discriminator audio at 32 kHz. Use a new filename and stop after a short capture for WAV replay. The decoder taps that same audio before speech filtering, de-emphasis and squelch. Combined mode masks recognized BSS intervals before sending the remaining speech to the recognizer. BSS-only mode runs without a speech model.

One `sdr.bss.v1` JSON event is written per verified BSS payload, with a ten-second window for identical repeats. Events carry the frequency, source, sender, audio sample offset and sample rate. The timestamp records the time the receiver or WAV replay decoded the packet. BSS uses Bell 202 (1200 baud, 1200/2200 Hz), NRZI, HDLC flags and bit stuffing, and little-endian CRC-16/X.25. The decoder requires a valid FCS before parsing the BSS marker and fields. `sdr.bss.diagnostic.v1` files preserve recovered bits from rejected candidates with a recognizable BSS marker or repeated HDLC preamble. [HTCommander modem source](https://github.com/Ylianst/HTCommander/blob/dd053f913bbb01c3357e6ebc46beeb750e55e252/src/lib/radio/modem_rx_worker.dart) · [Yodel modem](https://docs.rs/yodel/0.2.0/yodel/)

The location event preserves the raw hex and decodes signed 24-bit latitude and longitude at 30,000 units per degree, following SDRmm. A longer position field can include altitude in metres, speed in km/h, and heading in degrees. Short or out-of-range positions retain the raw bytes with `encoding: unresolved`. The reference location bytes `124F80DDAAF000640000005A` decode to 40° N, 75° W, 100 m, 0 km/h, and 90°. For an independent on-air check, record the radio or app's displayed GPS fix alongside captures at known locations. [SDRmm decoder](https://github.com/Newspicel/sdrmm/blob/fef5a0ecc4a16f08675fb55b15a5301562151914/crates/channels/src/bss/frame.rs) · [SDRmm position fixture](https://github.com/Newspicel/sdrmm/blob/fef5a0ecc4a16f08675fb55b15a5301562151914/crates/channels/src/bss/tests.rs)

A CRC-verified VR-N76 location packet included `0x8B` followed by four bytes before the sender TLV. The event preserves those five bytes as `vr_n76_header_hex`. The meaning of that header remains unresolved.

Replay the same discriminator audio through both workers with `gateway-wav`. Supply each squelch-open interval in milliseconds relative to the start of the recording, because the WAV contains FM discriminator samples without its RF power measurement. Leave at least the correlation timeout in the recording after the last speech interval.

```powershell
.\target\release\r3akt-sigil.exe gateway-wav .\vr-n76-discriminator.wav --rf-open 1000:5000 --rf-open 7000:11500 --backend parakeet --output-dir .\gateway-replay
```

Set `forward_bss = true` under `[lxmf]` to forward live BSS messages through the existing watcher. Forwarded chat includes sender and any BSS text. Coordinates and raw location bytes stay in the local event. Start the watcher before receiving the packets you want to forward.
