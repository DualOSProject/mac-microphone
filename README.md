# Mac Microphone

A virtual USB microphone for Windows, so Windows apps can record from a Mac's microphone.
It is the microphone part of [DualOS](https://dualos.app), which joins a Mac and a Windows PC.

Windows shows it as **Microphone (Mac Microphone)**, and any app can pick it like a real USB
microphone. No kernel driver of our own is needed:

- the device is a USB Audio Class 2.0 microphone (48 kHz, mono), emulated in software and
  served over USB/IP on `127.0.0.1` only, using [usbip-audio](https://github.com/network-audio/usbip-audio) (MIT);
- the Microsoft-certified [usbip-win2](https://github.com/vadimgrn/usbip-win2) client attaches it;
- Windows binds its own in-box USB audio driver (`usbaudio2.sys`).

The device exists while the host application runs: it is attached at start and detached at
exit. If the application ends unexpectedly, the USB/IP connection closes and Windows sees the
microphone unplugged.

## USB identity

`1209:D05C` from [pid.codes](https://pid.codes) (VID 0x1209). The serial number is the
computer's name, so Windows keeps one device and its settings per computer.

## Use

Rust: `Mic::start(name, serial)`, then `mic.feed().push(&samples)` with 48 kHz mono `i16`
samples. `mic.feed().in_use()` tells whether a Windows app is recording right now (the host
starts the real microphone only then).

C ABI (the `cdylib`, used by the DualOS Windows app):

| Function | |
|---|---|
| `dualos_mic_start(name, serial, error, error_capacity)` | attach; returns a handle or null with the reason |
| `dualos_mic_push(mic, samples, count)` | 48 kHz mono PCM16 |
| `dualos_mic_in_use(mic)` | 1 while a Windows app records |
| `dualos_mic_stats(mic, out)` | counters |
| `dualos_mic_stop(mic)` | detach and free |

A small adaptive buffer (15–80 ms) absorbs network jitter between the Mac and the PC.

## Test

With usbip-win2 installed, as administrator:

```
cargo run --release --bin mic_probe -- 30
```

attaches "Mac Microphone" with a 440 Hz test tone for 30 seconds.

## License

0BSD (BSD Zero Clause), see [LICENSE](LICENSE): use it for anything, no conditions.
Its dependency usbip-audio is MIT; usbip-win2 is BSD-2-Clause and installed separately.
