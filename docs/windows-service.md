# Running trunkd as a Windows service

How the recorder is installed, run, controlled, kept alive and upgraded on a Windows box, and how to
keep that box's clock good enough for recording. It assumes the install layout from
[install-windows.md](install-windows.md) (`C:\trunkd\bin`, `C:\trunkd\config\trunkd.toml`). The
Linux equivalent is a systemd unit ([install-linux.md](install-linux.md)).

Three pieces:

| piece | what it is | how it is started |
| --- | --- | --- |
| **`trunkd` service** | the recorder itself (`trunkd service run --config …`) | Service Control Manager, automatic (delayed) start |
| **`trunkd-monitor` service** (optional) | `trunkd monitor …`: polls the recorder's panel and restarts the service when it stops recording | SCM, automatic start, no dependency on the recorder |
| **SCM failure actions** | restart the recorder 5 s / 30 s / 60 s after it *exits* abnormally | the SCM itself, no process of ours involved |

The failure actions catch a process that dies. The monitor catches the cases they cannot: a process
that is alive but not recording — panel unreachable, control channel never acquired, a supervised
unit that exhausted its restart budget — and a service the SCM gave up restarting.

## Install, start, stop, remove

All from an **elevated** prompt (`trunkd.exe` refuses `install`/`uninstall` without it):

```powershell
cd C:\trunkd\bin
.\trunkd.exe service install --config C:\trunkd\config\trunkd.toml --monitor
.\trunkd.exe service start
.\trunkd.exe service status
.\trunkd.exe service stop
.\trunkd.exe service uninstall            # stops and removes both services
```

`install` registers an own-process service whose command line is
`"C:\trunkd\bin\trunkd.exe" service run --config "C:\trunkd\config\trunkd.toml"` (the config path
is canonicalised and must exist), description "P25 trunked-radio recorder (trunkd)", start type
automatic (delayed, so USB and the SDRplay service settle first), account LocalSystem, and the
failure actions above with a one-day reset. LocalSystem is fine: WinUSB and the SDRplay service are
reachable from session 0, and no network share or user profile is involved. `--name`,
`--display-name`, `--start-type demand` and `--account DOMAIN\user` (then
`sc.exe config <name> password=…`) are there for the unusual cases. `--monitor` also registers
`trunkd-monitor` (see [Keeping it alive](#keeping-it-alive)).

When the service starts it sets its working directory to the config's directory, logs
`trunkd starting … log_dir=… cwd=…` as its first line, opens the tuners, and acquires the control
channel. A service has no console, so it writes a rolling `trunkd.log` to `[server] log_dir`
(relative paths resolve against the config's directory), defaulting to a `logs` directory beside
the config. `service start` and `service stop` are thin SCM wrappers; `stop` waits for the
recorder's orderly teardown (open calls finished, call queue and uplink drained) with a 30 s wait
hint. `services.msc`, `sc.exe start/stop trunkd` and `Restart-Service trunkd` all do the same
thing.

The log directory and panel address are baked into the monitor's command line at install time, so
changing `[server] log_dir` or `[server] bind` afterwards needs `service uninstall` followed by
`install`.

## Commands it accepts while running

The recorder is headless; there is no console to type at. It takes commands three ways:

**The SCM** — start, stop, shutdown (at OS shutdown), and interrogate. A stop from any of the tools
above is the equivalent of one Ctrl+C: finish open calls, drain, exit 0.

**The local control panel** — `[server] enabled = true` in the config, on `127.0.0.1:8080` by
default. Every route is in [panel.md](panel.md); the ones you will use most:

```powershell
Invoke-RestMethod http://127.0.0.1:8080/api/status | ConvertTo-Json -Depth 4
Invoke-RestMethod -Method Post http://127.0.0.1:8080/api/tuners/sdr0/restart
Invoke-RestMethod -Method Post http://127.0.0.1:8080/api/uplink `
  -ContentType application/json -Body '{"spool_only": false}'   # release the upload hold
```

Expose it on the LAN only by setting `bind = "0.0.0.0:8080"` **and** `tokens`
([panel.md](panel.md#security)).

**Reloading the config does not restart the service.** `POST /api/config/reload` tears the
recording pipeline down through the ordinary teardown and builds it again inside the *same
process*: same PID, same log file, same service instance. The SCM is never told the service
stopped — it stays `RUNNING` throughout, no failure action fires, and the event log gets no 7031.
The health monitor does not see it as a failure either: the panel port is unbound for the couple of
seconds the rebuild takes, and the monitor only calls a panel unhealthy after a full window of
failed polls. The recorder log says `recorder reloading`, then `reloading configuration`, then the
usual startup lines. If the file has become unparseable in the gap between the reload being
accepted and the file being re-read, the recorder logs the error and carries on with the
configuration it already had rather than exiting.

**Keyup's desired state** — whenever `[uplink] enabled = true`, held or not, the recorder POSTs its
tuner status every ~15 s and polls desired state every ~10 s, and applies what Keyup pushes
([keyup.md](keyup.md#6-desired-state--what-keyup-can-push-down)). The upload hold can be released
from there too; the most recent writer wins.

## Keeping it alive

**Failure actions.** Killing the recorder's process makes the SCM log event 7031 and start a new
instance 5 s later. The second failure inside the reset period waits 30 s, every further one 60 s;
the counter resets after a day without one.

**The health monitor** — `trunkd monitor`, installed as the `trunkd-monitor` service by
`service install --monitor`, or run by hand:

```powershell
C:\trunkd\bin\trunkd.exe monitor --service trunkd --url http://127.0.0.1:8080 --log-dir C:\trunkd\logs --config C:\trunkd\config\trunkd.toml
```

**With panel tokens set** the monitor needs one to read `/api/status`. It uses, in order, `--token`,
`$TRUNKD_MONITOR_TOKEN`, then the first `[server] tokens` entry of the recorder's `--config` file —
and `service install --monitor` registers `--config` for it, so an installed monitor needs nothing
else. A panel that still refuses the monitor (401/403) is logged once as a misconfiguration and
counts as *answering*: the recorder is not restarted over it, but only the unreachable rule can
fire until it is fixed, because tuner and unit health cannot be read.

Every `--interval` (10 s) it reads `/api/status` and the SCM, and restarts the recorder service
(stop, wait ≤ 45 s, start) when a fault has persisted for its whole window — never on a single
failed poll:

| rule | fault | window |
| --- | --- | --- |
| unreachable | the panel does not answer (connection refused, timeout, a non-200 other than 401/403) | `--unreachable-after 60s` |
| no lock | every control-following tuner reports `locked = false`, including a recorder that never locked since it started. A tuner Keyup has disabled is skipped. | `--unlocked-after 10m` |
| failed unit | a `supervisor[]` entry is in its terminal `failed` state (restart budget exhausted). Device units do not get here: past their budget they stay `retrying` on a 30 s → 10 min backoff inside the process | `--failed-after 30s` |
| service down | the service is stopped with a non-zero exit code and the SCM has not brought it back | `--stopped-after 30s` → start it |

A recorder that an operator stopped (exit code 0) is left alone: the monitor logs once that it is
stopped cleanly and waits for `trunkd service start`, so `service stop` never starts a fight with
the operator. Back-off: never two restarts within `--min-interval` (2 min); a fault that returns
within 10 min of a restart doubles the wait each time up to `--max-interval` (30 min); 30 min of
health resets it. The monitor's own log is `C:\trunkd\logs\trunkd-monitor.log.YYYY-MM-DD` — every
decision, which rule fired, and what the restart returned. It has no SCM dependency on the
recorder, so it keeps running while the recorder is down, which is the point. `service status`
shows both services; `service uninstall` removes both.

On Linux the same binary runs the same rules with `--restart-cmd "systemctl restart trunkd"`
instead of `--service` ([operations.md](operations.md#health-monitor)).

## Where to look when something is wrong

* `trunkd service status` — state, pid, start type, account, command line.
* `C:\trunkd\logs\trunkd.log.<date>` — the recorder; the 10 s status line is the one to watch
  (`control=`, `tsbk_per_s`, `overflow`, `spooled`; `auto_ppm` steps appear as
  `auto frequency correction`). [operations.md](operations.md#what-the-log-says) explains it.
* `C:\trunkd\logs\trunkd-monitor.log.<date>` — why the monitor did what it did. Both logs go
  wherever `[server] log_dir` points (or `<config dir>\logs` when it is unset).
* A log that Explorer or `Get-ChildItem` shows as **0 bytes** while the service runs is not empty:
  NTFS does not refresh the directory entry's size until the writer closes the file. Read it
  (`Get-Content -Tail 50`) instead of trusting the size.
* `C:\trunkd\logs\trunkd-service-error.txt` — written only when the service could not even load its
  config or start logging.
* Event Viewer → System → *Service Control Manager*: 7036 start/stop, 7031 "terminated
  unexpectedly" with the corrective action; Application → *Application Error* 1000 if the process
  faulted (faulting module named).
* `sc.exe query trunkd` — `WIN32_EXIT_CODE 0` after a clean stop, 1067 after a crash or kill. A
  1067 on every *clean* stop with an Airspy attached is the libusb version problem
  ([sdr-hardware.md](sdr-hardware.md#windows-runtime)).

## Upgrading

### The update script

`scripts\windows\update-trunkd.ps1` pulls the branch you deploy (default `main`), and if there is
anything the recorder is not already running, builds it, tests it, installs it, restarts both
services and checks the tuners lock — rolling back if they do not. When nothing has changed it says
so and exits without building.

**Requirements**

* PowerShell 7 (`pwsh`). Windows PowerShell 5.1 will not run it.
* An elevated prompt to install. The service ACL lets only Administrators stop `trunkd`; without
  elevation the script still builds and tests, then stages the exe (below).
* A git checkout of trunkd — by default the one the script itself lives in (`-RepoDir` otherwise) —
  on the deploy branch, with no uncommitted changes to tracked files. Untracked files are fine.
* Rust at `%USERPROFILE%\.cargo\bin`; the script puts it on `PATH` itself.
* The install in `C:\trunkd` (`-InstallDir` otherwise), with the services registered under the
  default names `trunkd` and `trunkd-monitor` (`service install --monitor`).
* The local panel enabled (`[server] enabled = true`) and reachable at `http://127.0.0.1:8080`
  (`-PanelUrl` otherwise). The lock check reads `/api/status`; with the panel off it can never
  pass, and every deploy would be rolled back. If `[server] tokens` is set, pass one as
  `-PanelToken` or in `TRUNKD_PANEL_TOKEN`.

**Running it** (from the root of the checkout)

```powershell
pwsh -File scripts\windows\update-trunkd.ps1             # update if needed
pwsh -File scripts\windows\update-trunkd.ps1 -BuildOnly  # build and test only
Get-Help .\scripts\windows\update-trunkd.ps1 -Detailed   # every switch
```

The script is read whole before it runs, so the pull it does can update the script without
disturbing the run in progress; the new version applies next time.

**What a run does, in order**

1. **Fetch and fast-forward.** `git fetch origin <branch>`, then `git pull --ff-only`. It never
   resets, stashes or discards: uncommitted changes, a checkout on another branch, or a branch that
   has diverged from origin stop the run with a message. Local commits that are simply ahead of
   origin are allowed, with a warning that they are unpushed.
2. **Decide.** HEAD is compared with the commit in `C:\trunkd\state\deployed-commit.txt`. Equal
   means "up to date" and the run ends. A missing file — the first run on a box — counts as
   "deploy".
3. **Build and test.** `cargo build --release`, then `cargo test --release --workspace`. A test that
   fails is run once more on its own (`-- <name> --exact`): if it passes the second time it is
   logged as flaky and the run goes on; if it fails again the run stops. A compile error or a
   crashed test binary stops the run without a rerun. Nothing about the running recorder has
   changed at this point.
4. **Stage instead, if it cannot install.** Not elevated, or `-BuildOnly`: the new exe is copied to
   `C:\trunkd\bin\trunkd-staged.exe` and the script exits with code 2. Running it again elevated
   installs it (the build is reused, so that run is quick).
5. **Install.** The current exe is kept as `C:\trunkd\bin\trunkd.exe.prev`. `trunkd-monitor` is
   stopped first (it would otherwise restart the recorder it sees go down), then
   `trunkd service stop`, then the script waits for the installed `trunkd.exe` to exit, copies the
   new one over it and starts both services. If anything in that sequence fails, it starts the
   services again on whichever exe is in place before reporting the error, so a failed swap never
   leaves the recorder down.
6. **Verify.** It polls `/api/status` every 3 s for up to two minutes (`-LockTimeoutSec`) until
   every tuner that is enabled *and* assigned to a site reports `locked`. Tuners on no site are
   ignored: they are declared but idle and never lock.
7. **Roll back, or record.** If the tuners do not lock in time, the previous exe is put back and
   the services restarted (unless `-NoRollback`), and the run exits 1. If they lock, the commit, the
   exe's SHA-256 and the time are written to `deployed-commit.txt` and the staged copy, if any, is
   removed.

Only one run can be active at a time; a second one exits 3 straight away.

**Switches**

| switch | effect |
| --- | --- |
| `-BuildOnly` | build, test and stage; never touch the services |
| `-Force` | build and deploy even when HEAD is already the deployed commit |
| `-SkipTests` | skip `cargo test` (not recommended) |
| `-NoRollback` | if the tuners do not lock, leave the new exe in place |
| `-LockTimeoutSec <n>` | how long to wait for the lock after a restart (default 120) |
| `-PanelToken <t>` | bearer token for the panel (default `$env:TRUNKD_PANEL_TOKEN`) |
| `-RepoDir <dir>` | the checkout (default: the one the script lives in) |
| `-InstallDir <dir>` | the install (default `C:\trunkd`) |
| `-Branch <name>` | the branch to deploy (default `main`) |
| `-PanelUrl <url>` | the panel (default `http://127.0.0.1:8080`) |

**Exit codes**

| code | meaning |
| --- | --- |
| 0 | up to date, or deployed and the tuners locked |
| 1 | failed: nothing was changed, or the previous exe was put back |
| 2 | built, tested and staged as `trunkd-staged.exe`, not installed |
| 3 | another run is already in progress |

**Files it reads and writes**

| path | what |
| --- | --- |
| `C:\trunkd\logs\update-trunkd-YYYY-MM-DD.log` | one log per day; every step, and the full git and cargo output indented under it |
| `C:\trunkd\state\deployed-commit.txt` | `<commit> <exe sha256> <time>` of the last verified deploy; delete it, or use `-Force`, to redeploy the same commit |
| `C:\trunkd\bin\trunkd.exe.prev` | the exe that was running before the last install |
| `C:\trunkd\bin\trunkd-staged.exe` | a built and tested exe waiting for an elevated run |

**Running it on a schedule**

A public clone can be fetched without credentials, so a scheduled task does not need the account's
password for git. Register it from an elevated prompt; it runs as your own account (where Rust
lives) with highest privileges. Replace the path with your checkout's:

```powershell
$script    = 'D:\src\trunkd\scripts\windows\update-trunkd.ps1'
$action    = New-ScheduledTaskAction -Execute 'pwsh.exe' -Argument "-NoProfile -File $script"
$trigger   = New-ScheduledTaskTrigger -Once -At (Get-Date) -RepetitionInterval (New-TimeSpan -Hours 1)
$principal = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" `
               -LogonType S4U -RunLevel Highest
Register-ScheduledTask -TaskName 'trunkd update' -Action $action -Trigger $trigger -Principal $principal
```

`S4U` runs whether or not you are logged on. The task's *Last Run Result* is the script's exit code
(0x0, 0x1, 0x2, 0x3 as above).

**Rolling back by hand**

To go back to the build that was running before the last install, from an elevated prompt:

```powershell
Stop-Service trunkd-monitor
C:\trunkd\bin\trunkd.exe service stop
Copy-Item C:\trunkd\bin\trunkd.exe.prev C:\trunkd\bin\trunkd.exe -Force
C:\trunkd\bin\trunkd.exe service start
```

Then decide what the next scheduled or manual run should do. If you rolled back because the new
build is bad, leave `deployed-commit.txt` alone: the script still believes HEAD is installed, so it
waits for a newer commit instead of reinstalling the bad one. If you want the next run to install
HEAD again, delete `deployed-commit.txt` (or run with `-Force`).

**When a run stops**

| message | what to do |
| --- | --- |
| `no trunkd checkout found` | run the script from inside a checkout, or pass `-RepoDir` |
| `the checkout … has uncommitted changes` | commit or stash them in the checkout, then rerun |
| `checkout is on '…', expected 'main'` | `git switch main`, or pass `-Branch` |
| `local main has diverged from origin` | merge or rebase by hand; the script will not choose for you |
| `tests failed twice` | a real failure: the log names the tests; nothing was installed |
| `cargo test failed without a failing test to rerun` | a compile error or a crashed test binary; the log has cargo's errors |
| `staged … (not elevated)` | rerun from an elevated prompt to install |
| `trunkd.exe is still running 30 s after the services were stopped` | something else holds the installed exe; the services were restarted on the old one |
| `tuners did not lock within 120 s` | the new build was rolled back; check the recorder log for why it did not lock, or raise `-LockTimeoutSec` if the radios are slow to come up |

### By hand

The same steps without the script, from the root of the checkout:

```powershell
git pull; cargo build --release
C:\trunkd\bin\trunkd.exe service stop        # the recorder
Stop-Service trunkd-monitor                  # the monitor runs the SAME exe and holds it open
Copy-Item target\release\trunkd.exe C:\trunkd\bin\ -Force
C:\trunkd\bin\trunkd.exe service start       # starts the monitor again too
```

Both services must be down for the copy: `service stop` alone leaves `trunkd-monitor` running, and
Windows then refuses to overwrite `trunkd.exe` ("being used by another process"). The recorder
would come back on the old binary without any other sign that the upgrade did not happen — check
`Get-FileHash` on both files if in doubt.

Re-run `build-redist.ps1` and copy `target\redist\*` only when the SoapySDR side changed (new module
version, new SDRplay API). The service registration does not change with the binary; `uninstall` +
`install` is needed only to change the config path, account, names, log directory or panel address.

## Host clock and NTP

trunkd's timestamps are only as good as the Windows clock: each tuner's stream clock is anchored to
it and steered toward it, so a host clock 200 ms off puts every call and event 200 ms off
([operations.md](operations.md#timestamps-and-the-host-clock) explains the mechanism and what
depends on it). Windows' default time settings poll as rarely as once every nine hours, and a PC's
crystal drifts by tens of ppm with temperature, which in practice means hundreds of milliseconds of
error between corrections. The fix is to poll often against a good server. trunkd itself has no
NTP setting and needs none.

### Recommended settings

| Setting | Value | Why |
| --- | --- | --- |
| NtpServer | `ntp.example.org,0x8 ntp2.example.org,0xa` | Your best server in plain client mode (`0x8`), and a fallback (`0x2` + `0x8`) used only if it stops answering. Avoid the `0x1` (*SpecialInterval*) flag: it makes the client poll on its own long interval and ignore Min/MaxPollInterval. |
| Type | `NTP` | |
| MinPollInterval | `6` (2⁶ = 64 s) | Correct the clock every minute instead of every few hours. |
| MaxPollInterval | `6` | |
| UpdateInterval | `100` | Microsoft's documented value for high-accuracy time (*Support boundary for high-accuracy time*, Windows 10 1607 and later): adjust more often, in smaller steps. |
| FrequencyCorrectRate | `2` | Same source: learn the clock's frequency error faster. |

Leave the rest at their defaults (`PhaseCorrectRate` 1, `MaxAllowedPhaseOffset` 300 — offsets under
300 s are slewed rather than stepped, which is what trunkd's trim follows best).

**Which server.** A stratum-1 server on your own network (a GPS-disciplined NTP appliance or a
Raspberry Pi with a GPS hat) is ideal. Otherwise use the closest reliable server you have — your
router, or a regional `pool.ntp.org` zone. Do not mix a local stratum-1 server with public pool
servers: they only add internet-path jitter. Point every recorder on a site at the same server, or
the recorders disagree by the difference between their clocks.

**A domain-joined machine** takes these settings from Group Policy, and a domain policy overrides
local configuration: `w32tm /config` on the machine is overwritten at the next policy refresh. Set
them in the GPO instead, under *Computer Configuration → Policies → Administrative Templates →
System → Windows Time Service*: *Time Providers → Configure Windows NTP Client* (NtpServer, Type)
and *Global Configuration Settings* (the four intervals and rates). If that GPO also applies to
machines that should not change, put the settings in a separate GPO linked only to the recorders'
OU. Then, elevated:

```powershell
gpupdate /force
w32tm /resync /force
```

**A standalone machine** is configured locally, elevated:

```powershell
w32tm /config /manualpeerlist:"ntp.example.org,0x8 ntp2.example.org,0xa" /syncfromflags:manual
$k = 'HKLM:\SYSTEM\CurrentControlSet\Services\W32Time\Config'
Set-ItemProperty $k MinPollInterval 6
Set-ItemProperty $k MaxPollInterval 6
Set-ItemProperty $k UpdateInterval 100
Set-ItemProperty $k FrequencyCorrectRate 2
w32tm /config /update
w32tm /resync /force
```

### What it does to the recorder

* **No trunkd setting changes.** Its stamps stay on the same clock as its own log file, the service
  timers and everything else on the box.
* **The correction reaches trunkd gradually.** Windows slews small offsets, so the system clock
  moves toward the server over seconds to minutes, and each tuner's trim follows at up to 1 ms a
  second. A 200 ms correction is absorbed in a few minutes, with no resync, no stamp moving
  backwards and no call visibly stretched.
* **A step is handled too.** If the clock is ever *stepped* (an offset over
  `MaxAllowedPhaseOffset`, or a manual `w32tm /resync` after a long outage), a step over 1.5 s is a
  resync within seconds (a warning, plus a `CLOCK_RESYNC` record in the event log), and a smaller
  one is trimmed out.
* **Afterwards the tuners sit close to true time**: with the host within a few ms of the server and
  each tuner within ~20 ms of the host, two tuners agree to within a few tens of milliseconds.
* **What remains is the sample clock's own rate error**, now visible: a tuner's `clock_trim_ms`
  keeps growing at that rate (86 ms a day is 1 ppm), and `clock_error_ms` should stay within
  ±20 ms.

### Checking it

```powershell
w32tm /query /status    # Source <your server>,0x8; Poll Interval 6 (64s)
w32tm /query /peers     # your servers, polling at 6 (64s)
w32tm /stripchart /computer:ntp.example.org /samples:5 /dataonly   # offsets of a few ms
```

These work without elevation; `w32tm /query /configuration` and `w32tm /query /source` need it. On a
domain-joined machine, check that the policy reached it:

```powershell
Get-ItemProperty HKLM:\SOFTWARE\Policies\Microsoft\W32Time\Parameters | Select NtpServer, Type
Get-ItemProperty HKLM:\SOFTWARE\Policies\Microsoft\W32Time\Config |
    Select MinPollInterval, MaxPollInterval, UpdateInterval, FrequencyCorrectRate
```

**Root dispersion rises right after the change.** It is the service's own error estimate, which
starts high after a reconfiguration and shrinks with each successful poll. The stripchart offset is
the number that says how close the clock is. An offset drifting by a millisecond or two between
polls in the first minutes is the service still learning the clock's frequency error.

### When it is not working

| Symptom | Likely cause |
| --- | --- |
| `w32tm /query /peers` lists servers you removed | a policy refresh brought an old GPO back; check which GPO wins with `gpresult /h report.html` |
| `Poll Interval: 15 (32768s)` | a server flag still has `0x1` (SpecialInterval), or the interval settings are not applied |
| `Source: Local CMOS Clock` or `Free-running System Clock` | the servers are unreachable on UDP 123; check the firewall |
| Stripchart offsets over ~10 ms an hour after the change | the service is not correcting; `w32tm /query /status` shows the last successful sync time |
| A tuner's `clock_error_ms` stuck outside ±20 ms | the host clock is moving faster than the trim follows (over 1 ms a second), or samples are being lost (`stream_gaps` climbing) |
