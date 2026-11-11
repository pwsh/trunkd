# Windows scripts

`Get-Help <script> -Detailed` lists every switch.

| script | what it does | full instructions |
| --- | --- | --- |
| `update-trunkd.ps1` | Pulls `origin/main`; if the recorder is not already running it, builds, tests, installs, restarts both services, checks the tuners lock, and rolls back if they do not. Needs PowerShell 7 (`pwsh`). | [docs/windows-service.md, "The update script"](../../docs/windows-service.md#the-update-script) |
| `build-redist.ps1` | Builds the SoapySDR runtime (SoapySDR, libairspy, the Airspy and SDRplay modules) that `trunkd.exe` loads, from pinned sources. Rerun only when that side changes. | [docs/install-windows.md, "The SoapySDR runtime"](../../docs/install-windows.md#2-the-soapysdr-runtime) |

Both scripts find the checkout from their own location, so run them from inside it. The usual
upgrade, from an elevated PowerShell 7 prompt at the root of the checkout:

```powershell
pwsh -File scripts\windows\update-trunkd.ps1
```

`-RepoDir`, `-InstallDir` (default `C:\trunkd`), `-Branch` and `-PanelUrl` override the defaults;
`build-redist.ps1` takes `-SourceRoot` and `-OutDir` (defaults under the checkout's `target\`).

Exit codes: 0 up to date or deployed, 1 failed (nothing changed or rolled
back), 2 built and staged but not installed (not elevated, or `-BuildOnly`),
3 another run in progress. Each run logs to
`C:\trunkd\logs\update-trunkd-YYYY-MM-DD.log`.
