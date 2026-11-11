# SPDX-License-Identifier: GPL-3.0-or-later
<#
.SYNOPSIS
    Pull trunkd from origin/main, and if there is anything new: build, test,
    install the new trunkd.exe, restart both services and check the tuners lock.

.DESCRIPTION
    1. Fetches origin and fast-forwards the local checkout. A dirty or
       diverged checkout stops the run; nothing is ever reset or discarded.
    2. Compares HEAD with the commit last deployed (state\deployed-commit.txt).
       Equal means nothing to do. A missing state file means "deploy".
    3. cargo build --release, then cargo test --release --workspace.
       Tests that fail are run once more on their own; one that passes the
       second time is logged as flaky, one that fails twice stops the run
       here and the running recorder is not touched.
    4. Installing needs Administrator rights (the service ACL only lets
       Administrators stop trunkd). Unelevated, or with -BuildOnly, the new
       exe is staged as bin\trunkd-staged.exe and the script exits 2.
    5. Elevated: keeps the current exe as bin\trunkd.exe.prev, stops
       trunkd-monitor then trunkd, swaps the exe, starts both, then polls
       /api/status until every enabled tuner that has a site is locked.
       If that does not happen in time, the previous exe is put back.

    Every run is logged to logs\update-trunkd-YYYY-MM-DD.log.

.PARAMETER Force
    Build and deploy even if HEAD is already the deployed commit.
.PARAMETER BuildOnly
    Build and test, stage the exe, but do not touch the services.
.PARAMETER SkipTests
    Skip cargo test (not recommended).
.PARAMETER NoRollback
    If the tuners do not lock after the deploy, leave the new exe in place.
.PARAMETER LockTimeoutSec
    How long to wait for the tuners to lock after a restart. Default 120.
.PARAMETER PanelToken
    Bearer token for the local panel, if [server] tokens is set.
    Defaults to the TRUNKD_PANEL_TOKEN environment variable.

.PARAMETER RepoDir
    The trunkd checkout. Defaults to the one this script lives in
    (scripts\windows\..\..). Required when the script is run from a copy
    outside a checkout.
.PARAMETER InstallDir
    The install folder (bin\, config\, logs\, state\). Default C:\trunkd.
.PARAMETER Branch
    The branch to deploy. Default main.
.PARAMETER PanelUrl
    The recorder's local panel. Default http://127.0.0.1:8080.

.EXAMPLE
    pwsh -File scripts\windows\update-trunkd.ps1
    From the root of the checkout: update if needed.
.EXAMPLE
    pwsh -File scripts\windows\update-trunkd.ps1 -BuildOnly
.EXAMPLE
    pwsh -File D:\src\trunkd\scripts\windows\update-trunkd.ps1 -InstallDir D:\trunkd

.NOTES
    Full instructions, scheduling, rollback and troubleshooting:
    docs\windows-service.md, "Upgrading".

    Run from an elevated PowerShell 7 prompt to install. The script is read
    whole before it runs, so the pull it does cannot change it mid-run.

    Exit codes: 0 up to date or deployed and verified
                1 failed (nothing changed, or rolled back)
                2 built and staged, not installed (not elevated / -BuildOnly)
                3 another run is already in progress
#>
#Requires -Version 7
[CmdletBinding()]
param(
    [switch]$Force,
    [switch]$BuildOnly,
    [switch]$SkipTests,
    [switch]$NoRollback,
    [int]$LockTimeoutSec = 120,
    [string]$PanelToken = $env:TRUNKD_PANEL_TOKEN,
    [string]$RepoDir = $(
        $up = Join-Path $PSScriptRoot '..\..'
        if (Test-Path (Join-Path $up 'Cargo.toml')) { (Resolve-Path $up).Path } else { '' }
    ),
    [string]$InstallDir = 'C:\trunkd',
    [string]$Branch = 'main',
    [string]$PanelUrl = 'http://127.0.0.1:8080'
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if (-not $RepoDir -or -not (Test-Path (Join-Path $RepoDir 'Cargo.toml'))) {
    throw "no trunkd checkout found: run the script from inside one, or pass -RepoDir <checkout>"
}

$BinDir     = Join-Path $InstallDir 'bin'
$Exe        = Join-Path $BinDir 'trunkd.exe'
$PrevExe    = Join-Path $BinDir 'trunkd.exe.prev'
$StagedExe  = Join-Path $BinDir 'trunkd-staged.exe'
$StateFile  = Join-Path $InstallDir 'state\deployed-commit.txt'
$BuiltExe   = Join-Path $RepoDir 'target\release\trunkd.exe'
$LogFile    = Join-Path $InstallDir ("logs\update-trunkd-{0:yyyy-MM-dd}.log" -f (Get-Date))
$CargoBin   = Join-Path $env:USERPROFILE '.cargo\bin'

# ---------------------------------------------------------------- helpers

function Write-Log {
    param([string]$Message, [string]$Level = 'INFO')
    $line = '{0:yyyy-MM-dd HH:mm:ss} {1,-5} {2}' -f (Get-Date), $Level, $Message
    Add-Content -Path $LogFile -Value $line
    switch ($Level) {
        'ERROR' { Write-Host $line -ForegroundColor Red }
        'WARN'  { Write-Host $line -ForegroundColor Yellow }
        'OK'    { Write-Host $line -ForegroundColor Green }
        default { Write-Host $line }
    }
}

# Run a native command; log its output; throw on a non-zero exit unless allowed.
function Invoke-Native {
    param(
        [Parameter(Mandatory)][string]$File,
        [string[]]$Arguments = @(),
        [string]$WorkDir = $RepoDir,
        [switch]$AllowFailure,
        [switch]$Quiet
    )
    Push-Location $WorkDir
    try {
        $output = & $File @Arguments 2>&1 | ForEach-Object { "$_" }
        $code = $LASTEXITCODE
    } finally {
        Pop-Location
    }
    if ($output) { Add-Content -Path $LogFile -Value ($output | ForEach-Object { "    $_" }) }
    if (-not $Quiet -and $output) { $output | Select-Object -Last 3 | ForEach-Object { Write-Host "    $_" } }
    if ($code -ne 0 -and -not $AllowFailure) {
        throw "$File $($Arguments -join ' ') failed with exit code $code"
    }
    [pscustomobject]@{ Code = $code; Output = @($output) }
}

function Get-Git {
    param([string[]]$Arguments)
    # git's "LF will be replaced by CRLF" notices arrive on stderr; they are not output.
    ((Invoke-Native -File 'git' -Arguments $Arguments -Quiet).Output |
        Where-Object { $_ -notmatch '^warning: ' }) -join "`n"
}

function Test-Elevated {
    $id = [Security.Principal.WindowsIdentity]::GetCurrent()
    ([Security.Principal.WindowsPrincipal]$id).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Get-PanelStatus {
    $headers = @{}
    if ($PanelToken) { $headers['Authorization'] = "Bearer $PanelToken" }
    Invoke-RestMethod -Uri "$PanelUrl/api/status" -Headers $headers -TimeoutSec 5
}

# Every enabled tuner assigned to a site must be locked. Tuners with no site
# (the wideband Airspy and the RSPdx) are declared but idle and never lock.
function Wait-TunersLocked {
    param([int]$TimeoutSec)
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    $last = 'panel not answering yet'
    while ((Get-Date) -lt $deadline) {
        try {
            $s = Get-PanelStatus
            $want = @($s.tuners | Where-Object { $_.enabled -and $_.site })
            $locked = @($want | Where-Object { $_.locked })
            $last = "{0} of {1} site tuners locked" -f $locked.Count, $want.Count
            if ($want.Count -gt 0 -and $locked.Count -eq $want.Count) {
                Write-Log "tuners locked: $last ($(($want | ForEach-Object { $_.role }) -join ', '))" 'OK'
                return $true
            }
        } catch {
            $last = "panel: $($_.Exception.Message)"
        }
        Start-Sleep -Seconds 3
    }
    Write-Log "tuners did not lock within $TimeoutSec s: $last" 'ERROR'
    return $false
}

function Stop-Trunkd {
    Write-Log 'stopping trunkd-monitor and trunkd'
    # The monitor goes first: it would otherwise restart the recorder it sees stop.
    if ((Get-Service trunkd-monitor).Status -ne 'Stopped') { Stop-Service trunkd-monitor }
    Invoke-Native -File $Exe -Arguments @('service', 'stop') -WorkDir $BinDir -AllowFailure | Out-Null
    # Both services run the installed trunkd.exe; it cannot be replaced until
    # both exit. A trunkd.exe run from elsewhere (the build folder) is ignored.
    $installed = {
        Get-Process -Name trunkd -ErrorAction SilentlyContinue |
            Where-Object { -not $_.Path -or $_.Path -eq $Exe }
    }
    $deadline = (Get-Date).AddSeconds(30)
    while ((& $installed) -and (Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 500
    }
    if (& $installed) {
        throw 'trunkd.exe is still running 30 s after the services were stopped'
    }
}

function Start-Trunkd {
    Write-Log 'starting trunkd and trunkd-monitor'
    Invoke-Native -File $Exe -Arguments @('service', 'start') -WorkDir $BinDir | Out-Null
    if ((Get-Service trunkd-monitor).Status -ne 'Running') { Start-Service trunkd-monitor }
}

# Copy with a few retries: Windows can hold the image briefly after exit.
function Copy-Exe {
    param([string]$From, [string]$To)
    for ($i = 1; $i -le 5; $i++) {
        try { Copy-Item -Path $From -Destination $To -Force; return }
        catch { if ($i -eq 5) { throw }; Start-Sleep -Seconds 2 }
    }
}

# ---------------------------------------------------------------- main

New-Item -ItemType Directory -Force -Path (Split-Path $LogFile), (Split-Path $StateFile) | Out-Null

# One run at a time, so a scheduled run cannot collide with a manual one.
$mutex = [System.Threading.Mutex]::new($false, 'Global\trunkd-update')
if (-not $mutex.WaitOne(0)) {
    Write-Host 'another update-trunkd run is in progress' -ForegroundColor Yellow
    exit 3
}

function Invoke-Update {
    Write-Log "---- update-trunkd started (elevated: $(Test-Elevated))"
    $env:PATH = "$CargoBin;$env:PATH"

    # 1. Fetch and fast-forward. Never reset, stash or discard anything.
    $current = Get-Git @('rev-parse', '--abbrev-ref', 'HEAD')
    if ($current -ne $Branch) { throw "checkout is on '$current', expected '$Branch'" }
    if (Get-Git @('status', '--porcelain', '--untracked-files=no')) {
        throw "the checkout in $RepoDir has uncommitted changes; commit or stash them first"
    }

    $fetch = Invoke-Native -File 'git' -Arguments @('fetch', 'origin', $Branch) -AllowFailure -Quiet
    if ($fetch.Code -ne 0) {
        Write-Log 'fetch failed, retrying once' 'WARN'
        Invoke-Native -File 'git' -Arguments @('fetch', 'origin', $Branch) -Quiet | Out-Null
    }

    $behind = [int](Get-Git @('rev-list', '--count', "HEAD..origin/$Branch"))
    $ahead  = [int](Get-Git @('rev-list', '--count', "origin/$Branch..HEAD"))
    if ($ahead -gt 0 -and $behind -gt 0) {
        throw "local $Branch has diverged from origin ($ahead ahead, $behind behind); merge it by hand"
    }
    if ($ahead -gt 0) { Write-Log "local $Branch is $ahead commit(s) ahead of origin (unpushed)" 'WARN' }
    if ($behind -gt 0) {
        Write-Log "$behind new commit(s) on origin/${Branch}:"
        (Get-Git @('log', '--oneline', "HEAD..origin/$Branch")) -split "`n" |
            ForEach-Object { Write-Log "    $_" }
        Invoke-Native -File 'git' -Arguments @('pull', '--ff-only', 'origin', $Branch) -Quiet | Out-Null
    }

    # 2. Is HEAD already what is installed?
    $head = Get-Git @('rev-parse', 'HEAD')
    $short = $head.Substring(0, 7)
    $deployed = if (Test-Path $StateFile) { ((Get-Content $StateFile -TotalCount 1) -split '\s+')[0] } else { '' }
    if ($deployed -eq $head -and -not $Force) {
        Write-Log "up to date: $short is already deployed" 'OK'
        return 0
    }
    $from = if ($deployed) { $deployed.Substring(0, [Math]::Min(7, $deployed.Length)) } else { 'unknown' }
    Write-Log "deploying $short (installed: $from)"

    # 3. Build and test. A failure here leaves the recorder untouched.
    Write-Log 'cargo build --release'
    Invoke-Native -File 'cargo' -Arguments @('build', '--release') | Out-Null
    if ($SkipTests) {
        Write-Log 'tests skipped (-SkipTests)' 'WARN'
    } else {
        Write-Log 'cargo test --release --workspace'
        $t = Invoke-Native -File 'cargo' -Arguments @('test', '--release', '--workspace') -AllowFailure -Quiet
        $passed = ($t.Output | Select-String 'test result: \w+\. (\d+) passed' -AllMatches).Matches |
            ForEach-Object { [int]$_.Groups[1].Value } | Measure-Object -Sum
        if ($t.Code -ne 0) {
            # "test some::path ... FAILED" -> some::path
            $failed = @($t.Output | ForEach-Object {
                if ($_ -match '^test (\S+) \.\.\. FAILED$') { $Matches[1] } } | Sort-Object -Unique)
            if ($failed.Count -eq 0) {
                # Not a test failure (a compile error, a crashed test binary): no rerun.
                $t.Output | Where-Object { $_ -match '^error' } | Select-Object -First 5 |
                    ForEach-Object { Write-Log "    $_" 'ERROR' }
                throw 'cargo test failed without a failing test to rerun; not deploying'
            }
            Write-Log "$($failed.Count) test(s) failed; running them once more on their own" 'WARN'
            $failed | ForEach-Object { Write-Log "    $_" 'WARN' }
            $retry = Invoke-Native -File 'cargo' -Quiet -AllowFailure -Arguments (
                @('test', '--release', '--workspace', '--') + $failed + @('--exact'))
            if ($retry.Code -ne 0) {
                $still = @($retry.Output | ForEach-Object {
                    if ($_ -match '^test (\S+) \.\.\. FAILED$') { $Matches[1] } } | Sort-Object -Unique)
                $still | ForEach-Object { Write-Log "    still failing: $_" 'ERROR' }
                throw "tests failed twice ($($still.Count) failing); not deploying"
            }
            Write-Log "passed on the rerun, so treated as flaky: $($failed -join ', ')" 'WARN'
        }
        Write-Log "tests passed: $($passed.Sum)" 'OK'
    }
    $newHash = (Get-FileHash $BuiltExe).Hash

    # 4. Stage instead of installing when we cannot (or were told not to).
    if ($BuildOnly -or -not (Test-Elevated)) {
        Copy-Exe -From $BuiltExe -To $StagedExe
        $why = if ($BuildOnly) { '-BuildOnly' } else { 'not elevated' }
        Write-Log "staged $short as $StagedExe ($why). To install, rerun this script from an elevated prompt." 'WARN'
        return 2
    }

    # 5. Install, restart, verify; roll back if the tuners do not lock.
    if ((Test-Path $Exe) -and (Get-FileHash $Exe).Hash -eq $newHash) {
        Write-Log 'installed exe is already byte-identical to this build; recording it as deployed'
    } else {
        Copy-Exe -From $Exe -To $PrevExe
        Write-Log "previous exe kept as $PrevExe"
        try {
            Stop-Trunkd
            Copy-Exe -From $BuiltExe -To $Exe
            Start-Trunkd
        } catch {
            # Never leave the recorder down: start it on whatever exe is in place.
            Write-Log "swap failed: $($_.Exception.Message); restarting the services" 'ERROR'
            try { Start-Trunkd } catch { Write-Log "restart also failed: $($_.Exception.Message)" 'ERROR' }
            throw
        }

        if (-not (Wait-TunersLocked -TimeoutSec $LockTimeoutSec)) {
            if ($NoRollback) {
                throw "new build is installed but the tuners did not lock (-NoRollback: left in place)"
            }
            Write-Log 'rolling back to the previous exe' 'WARN'
            Stop-Trunkd
            Copy-Exe -From $PrevExe -To $Exe
            Start-Trunkd
            $ok = Wait-TunersLocked -TimeoutSec $LockTimeoutSec
            throw ("rolled back to the previous build; tuners {0}" -f $(if ($ok) { 'locked again' } else { 'STILL NOT LOCKED - check the recorder' }))
        }
    }

    Set-Content -Path $StateFile -Value ("{0} {1} {2:yyyy-MM-ddTHH:mm:ssK}" -f $head, $newHash, (Get-Date))
    if (Test-Path $StagedExe) { Remove-Item $StagedExe -Force }
    Write-Log "deployed $short" 'OK'
    return 0
}

$exitCode = 1
try {
    $exitCode = Invoke-Update
}
catch {
    Write-Log $_.Exception.Message 'ERROR'
    $exitCode = 1
}
finally {
    Write-Log "---- update-trunkd finished (exit $exitCode)"
    $mutex.ReleaseMutex()
    $mutex.Dispose()
}
exit $exitCode
