# usbwhere

`lsusb` tells you a USB device is plugged in, but not **where to access it**.
`usbwhere` shows, for each USB device and each of its interfaces, the driver bound to it and the matching nodes: `/dev/sdb1` (with its mount point), `/dev/video0`, `/dev/ttyACM0`, a network interface, a sound card...

## Build

You need a Rust toolchain and the libudev headers:

```sh
# Debian / Ubuntu
sudo apt install libudev-dev pkg-config

cargo build --release
```

The binary is `target/release/usbwhere`.

## Usage

```sh
usbwhere            # list current USB devices
usbwhere --all      # also show hubs
usbwhere --watch    # listen to udev, print plug/unplug events
usbwhere --no-color # disable colors (NO_COLOR is honored too)
```

## Example

```
Bus 001 Device 007  046d:0825  Logitech Webcam C270
   480 Mb/s · raw access (libusb): /dev/bus/usb/001/007
 ├─ Interface 0 · Video (UVC) · driver=uvcvideo
 │    → /dev/video0  [crw-rw----+ root:video]
 │    → /dev/video1  [crw-rw----+ root:video]
 └─ Interface 2 · Audio · driver=snd-usb-audio
      → sound card card2 (ALSA)

Bus 002 Device 003  0781:5581  SanDisk Ultra
   5000 Mb/s · raw access (libusb): /dev/bus/usb/002/003
 └─ Interface 0 · Mass storage · driver=usb-storage
      → /dev/sdb   [brw-rw---- root:disk]
      → /dev/sdb1  [brw-rw---- root:disk]  → mounted on /media/user/USB
```

An interface with no driver is flagged with `⚠ no driver bound`.

## How it works

- State is read from `/sys/bus/usb/devices` (no `lsusb` needed).
- For each interface, it walks its sysfs tree and reads the `DEVNAME=` entry of every `uevent` file to find the `/dev` nodes.
- Mount points come from `/proc/mounts`; vendor/product names fall back to `usb.ids` when the device doesn't provide them.
- `--watch` uses udev events and waits a short moment after a plug so that drivers have time to create their nodes.

## Limitations

- Mounting a disk doesn't trigger a udev event: the mount point may be missing on first display, run `usbwhere` again.
- `--watch` needs a running udev daemon (it won't work in most containers).
- Linux only.
