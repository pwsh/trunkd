# USB access to the SDR

trunkd talks to the radio as an ordinary user. It needs read/write access to the
radio's USB device node (and, for a HackRF in a PortaPack, its serial console). trunkd
ships no udev rules of its own: the driver packages already install them, and they
grant access to the `plugdev` group, which `scripts/systemd/trunkd.service` joins.

| Radio | Package that installs the rule (Debian/Ubuntu names) | Rule file |
|---|---|---|
| HackRF One | `hackrf` / `libhackrf0` | `/lib/udev/rules.d/60-libhackrf0.rules` |
| Airspy R2 / Mini | `libairspy0` (or `airspy`) | `/lib/udev/rules.d/60-libairspy0.rules` |
| RTL-SDR | `rtl-sdr` / `librtlsdr0` | `/lib/udev/rules.d/60-librtlsdr0.rules` |
| SDRplay | the SDRplay API installer | the API's own service owns the device; no group needed |

File names vary between distributions and releases; `ls /lib/udev/rules.d /etc/udev/rules.d | grep -i -e hackrf -e airspy -e rtl` shows what is installed.

After installing a package, or adding a user to `plugdev`:

```sh
sudo udevadm control --reload-rules && sudo udevadm trigger
sudo usermod -aG plugdev "$USER"   # only for running trunkd as yourself; log in again
```

Then unplug and replug the radio and check with `trunkd check --config <file>`, which
lists every SDR it can enumerate.

## If your distribution has no rule

Write `/etc/udev/rules.d/60-sdr.rules` with one line per radio, using the USB ids from
`lsusb`:

```
# HackRF One
SUBSYSTEM=="usb", ATTRS{idVendor}=="1d50", ATTRS{idProduct}=="6089", MODE="0660", GROUP="plugdev"
# Airspy R2 / Mini
SUBSYSTEM=="usb", ATTRS{idVendor}=="1d50", ATTRS{idProduct}=="60a1", MODE="0660", GROUP="plugdev"
# RTL2832U dongles
SUBSYSTEM=="usb", ATTRS{idVendor}=="0bda", ATTRS{idProduct}=="2838", MODE="0660", GROUP="plugdev"
```

## RTL-SDR: the DVB driver

The kernel's DVB-T driver claims RTL2832U dongles first. Blacklist it:

```sh
echo 'blacklist dvb_usb_rtl28xxu' | sudo tee /etc/modprobe.d/blacklist-rtl-sdr.conf
sudo modprobe -r dvb_usb_rtl28xxu
```

## PortaPack serial console

`[[device]] portapack_port` (e.g. `/dev/ttyACM0`) is a CDC-ACM serial device owned by the
`dialout` group on most distributions; the system unit already joins it.
