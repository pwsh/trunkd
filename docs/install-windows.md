# Installing trunkd on Windows

trunkd runs on 64-bit Windows 10/11 as a console program or, for a recorder that should keep
running, as a Windows service. This page covers getting the binary, the SDR runtime it needs, and
the install layout; running it as a service, upgrading it and keeping its clock right are in
[windows-service.md](windows-service.md). Linux is covered in [install-linux.md](install-linux.md).

## 1. Get `trunkd.exe`

**From a release.** Tagged releases on <https://github.com/pwsh/trunkd/releases> carry a Windows
x64 `trunkd.exe`. You still need the SDR runtime (step 2).

**From source.** Everything below runs in PowerShell.

```powershell
# 1. Toolchain
winget install Rustlang.Rustup            # or https://rustup.rs
winget install Microsoft.VisualStudio.2022.BuildTools --override "--quiet --wait --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
winget install Git.Git
# open a new shell so PATH picks them up
rustup show                               # rust-toolchain.toml selects the toolchain

# 2. Source
git clone https://github.com/pwsh/trunkd.git
cd trunkd

# 3. Build (default features: iqfile + hackrf + soapy — none of them needs a native
#    library at build time; SoapySDR is loaded at run time)
cargo build --release
# -> target\release\trunkd.exe

# 4. Sanity checks that need no radio
.\target\release\trunkd.exe --help
cargo test --release --workspace
```

A clean release build takes a couple of minutes on a recent machine.

**Why the Build Tools.** `trunkd-call` depends on `mp3lame-encoder`, which compiles LAME from
vendored C source through the `cc` crate — the only C code in the build. Use the default
`x86_64-pc-windows-msvc` target with Visual Studio Build Tools ("Desktop development with C++");
`cc` finds `cl.exe` on its own. The GNU target with MSYS2 mingw-w64 also works for `cc` but is less
tested.

**Likely first failures:**

* **`cc` cannot find a C compiler** while building `mp3lame-sys`: install the Build Tools workload
  above and open a new shell.
* **A crash (`STATUS_HEAP_CORRUPTION`, `0xC0000374`, or `0xC0000005`) the moment a Soapy device
  opens or closes.** Check the libusb version beside libairspy first
  ([sdr-hardware.md](sdr-hardware.md#windows-runtime)). To see where it dies, run it under `cdb`
  (`winget install Microsoft.WinDbg`) with `-g -G -c "g; kb 40; q"`: cargo writes `trunkd.pdb` next
  to the release exe, so the stack names the call.
* **Spool on two drives.** The spool relies on `rename` being atomic, which NTFS guarantees only
  within one volume. Keep `spool_path` on one drive.

## 2. The SoapySDR runtime

Airspy, SDRplay and RTL-SDR radios are driven through SoapySDR, which trunkd loads at run time.
There are two ways to provide it:

### The self-contained bundle (recommended)

`scripts\windows\build-redist.ps1` builds SoapySDR, libairspy, the Airspy module and the SDRplay
module from pinned upstream sources with the local Visual Studio toolchain, into one folder that
sits beside `trunkd.exe`:

```powershell
cd <your trunkd checkout>
powershell -File scripts\windows\build-redist.ps1        # -> target\redist (about 5 min cold, under 1 min warm)
Get-Help .\scripts\windows\build-redist.ps1 -Detailed    # -SourceRoot, -OutDir, -Configuration
```

It needs Git, the Visual Studio 2022 Build Tools (its bundled CMake is used deliberately), 7-Zip
for the libusb download, and the SDRplay API 3.x installed if you want the SDRplay module. Sources
and build trees go under `-SourceRoot` (default: `target\redist-src` in the checkout); the result
under `-OutDir` (default: `target\redist`). It pins libusb to 1.0.24 on purpose
([sdr-hardware.md](sdr-hardware.md#windows-runtime) explains the Airspy shutdown crash newer libusb
causes) and does not copy the VC++ runtime: the modules pass `std::string` across DLL boundaries, so
every binary must share the one system CRT.

trunkd finds `SoapySDR.dll` beside itself, and because that folder also holds
`SoapySDR\modules0.8\`, it sets `SOAPY_SDR_PLUGIN_PATH` to it before loading the library (SoapySDR
on Windows would otherwise look two directories up for its modules and find nothing). An operator's
own `SOAPY_SDR_PLUGIN_PATH` is never overridden. PothosSDR is then not needed at all; if it is
installed and on `PATH` it is simply not used, since the exe's own directory wins the DLL search.

### PothosSDR

Alternatively install PothosSDR and put its `bin` directory on `PATH`; see
[sdr-hardware.md](sdr-hardware.md#windows-runtime) for its limits (stale SDRplay module).

### Drivers and prerequisites

* Microsoft Visual C++ 2015–2022 x64 Redistributable
  (`winget install Microsoft.VCRedist.2015+.x64`).
* **WinUSB** bound to each Airspy, RTL-SDR and HackRF with Zadig (<https://zadig.akeo.ie/>), or by
  any prior SDR# / sdrtrunk install. **Not** for an SDRplay.
* The SDRplay API service for an RSP (`sdrplay_api.dll` in the bundle is only the client library).
* For a **HackRF**: `hackrf_transfer.exe`, `hackrf_info.exe` and `hackrf.dll` on `PATH` or beside
  `trunkd.exe` (from the official HackRF release zip). For a PortaPack, set `portapack_port` to the
  `COMn` port Device Manager shows for its console ([sdr-hardware.md](sdr-hardware.md#hackrf-one)).

## 3. Install layout

One directory, nothing on `PATH`, nothing in the registry but the service entries:

```text
C:\trunkd\
  bin\                       the self-contained redistributable + trunkd.exe
    trunkd.exe
    SoapySDR.dll, airspy.dll, libusb-1.0.dll, sdrplay_api.dll
    SoapySDR\modules0.8\airspySupport.dll, sdrPlaySupport.dll
    SoapySDRUtil.exe, airspy_info.exe, airspy_rx.exe    (diagnostics)
  config\trunkd.toml         the config (holds the upload token)
  logs\trunkd.log.YYYY-MM-DD daily-rolling log ([server] log_dir)
  spool\                     [uplink] spool_path
  events\                    [recorder] event_log_dir
  state\                     ppm memory; deployed-commit.txt for the update script
```

`C:\trunkd` is the default the update script expects (`-InstallDir` changes it). Build the folder
from your checkout:

```powershell
cd <your trunkd checkout>
cargo build --release
powershell -File scripts\windows\build-redist.ps1
New-Item -ItemType Directory -Force C:\trunkd\bin, C:\trunkd\config, C:\trunkd\logs, C:\trunkd\spool, C:\trunkd\events, C:\trunkd\state
Copy-Item target\redist\* C:\trunkd\bin -Recurse -Force
Copy-Item target\release\trunkd.exe C:\trunkd\bin\
Copy-Item trunkd.example.toml C:\trunkd\config\trunkd.toml
```

Then edit `C:\trunkd\config\trunkd.toml` ([configuration.md](configuration.md)):

* `[uplink] url`, `api_key`, `system_label`, `site_label`; your `[[device]]` and `[[site]]`;
* `[recorder] instance_id`, a stable name for this box — give every recorder its own;
* Windows paths, with **forward slashes** (a backslash is a TOML escape):
  `[server] log_dir = "C:/trunkd/logs"`, `[uplink] spool_path = "C:/trunkd/spool"`,
  `[recorder] event_log_dir = "C:/trunkd/events"`;
* `ring_seconds = 2.0` on each `[[device]]`: Windows has no real-time priority for the DSP thread,
  so the ring is the only thing absorbing scheduler hiccups;
* `[server] enabled = true` — the service's health monitor and the update script both read the
  local panel;
* `[uplink] spool_only = true` until the recorder has been watched recording. That holds *uploads*
  only — the Keyup control plane still connects, so the recorder shows up in Keyup from its first
  run and the hold can be released from there or from the local panel rather than by editing this
  file ([operations.md](operations.md#bring-up-record-without-uploading)).

Confirm with:

```powershell
C:\trunkd\bin\trunkd.exe --config C:\trunkd\config\trunkd.toml check
```

which must print `SoapySDR: C:\trunkd\bin\SoapySDR.dll (…)` and list the radios. A radio another
program (sdrtrunk, SDR#) has open is not listed ([sdr-hardware.md](sdr-hardware.md#only-free-radios-enumerate)).

## 4. First run

```powershell
C:\trunkd\bin\trunkd.exe --config C:\trunkd\config\trunkd.toml run
```

Look for `control channel acquired` in the log, then the 10 s status line
([operations.md](operations.md#what-the-log-says)). `DSP thread running at real-time priority` will
*not* appear on Windows; that is expected. Ctrl+C stops it cleanly.

Once capture is verified, install it as a service: [windows-service.md](windows-service.md).

## Notes

* **Paths in the TOML** need forward slashes (`"C:/trunkd/spool"`) or doubled backslashes.
* **Two recorders on one site** are fine for Keyup (it keeps the better copy of each call); give
  each its own `[recorder] instance_id`. To have them share tuners rather than duplicate them, see
  [recorder-link.md](recorder-link.md).
* **What is portable.** Everything OS-specific has a Windows branch: the spool free-space floor uses
  `GetDiskFreeSpaceExW`, Ctrl+C and service stops share one shutdown path, HTTP uses rustls (no
  OpenSSL), and the panel works the same. Real-time priority is Linux-only.
* **Conventional channels** (`[[channel]]`, analog NBFM with CTCSS/DCS) are pure Rust like the
  rest of the DSP: nothing extra to install, the same `trunkd.exe` and SoapySDR runtime record them.
  A tuner carrying only channels never logs `control channel acquired`; look for its calls on the
  panel's channels table and in the `calls.conventional_*` metrics instead. Wide-coverage radios such as the SDRplay RSPdx
  suit VHF/UHF channel work: they need the SDRplay API service and the SDRplay Soapy module (both in
  section 2), and their IF filter steps are often narrower than the sample rate, so keep channels
  inside the filter ([sdr-hardware.md](sdr-hardware.md#tuners-for-conventional-channels)).
* **Clock.** Windows' default time settings poll far too rarely for recording; see
  [windows-service.md](windows-service.md#host-clock-and-ntp).
