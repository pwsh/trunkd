# SPDX-License-Identifier: GPL-3.0-or-later
<#
.SYNOPSIS
    Builds a self-contained Windows x64 redistributable of the SoapySDR runtime
    that trunkd.exe needs, from pinned upstream sources, using the local
    VS 2022 toolchain. Replaces any dependency on the PothosSDR 2021 install.

.DESCRIPTION
    Components (all built from source except libusb and sdrplay_api.dll):

      libusb           1.0.24              prebuilt MSVC x64 binary from the
                                           libusb GitHub release (.7z).
                                           DELIBERATELY NOT the newest release;
                                           see WORKAROUND 3 below.
      pthreads-win32   3.1.0               STATIC lib, linked into airspy.dll
                                           (libairspy 1.0.10 needs pthreads on
                                           Windows; see WORKAROUND 1 below)
      SoapySDR         soapy-sdr-0.8.1     runtime + SoapySDRUtil.exe
      libairspy        v1.0.10             airspy.dll + airspy_info/airspy_rx
      SoapyAirspy      soapy-airspy-0.2.0  airspySupport.dll
      SoapySDRPlay3    soapy-sdrplay3-0.5.2 sdrPlaySupport.dll
      sdrplay_api.dll  (whatever the local SDRplay API 3.x install provides)

    Output layout (-OutDir, default target\redist in the checkout this script
    lives in; sources and build trees go to -SourceRoot, default
    target\redist-src beside it):

      redist\
        SoapySDR.dll
        SoapySDRUtil.exe
        airspy.dll
        airspy_info.exe
        airspy_rx.exe
        libusb-1.0.dll
        sdrplay_api.dll
        README.txt
        SoapySDR\modules0.8\airspySupport.dll
        SoapySDR\modules0.8\sdrPlaySupport.dll

    trunkd.exe is dropped into that folder. No vcruntime/msvcp DLLs are copied:
    the modules pass std::string across the DLL boundary, so every binary must
    share ONE CRT. The VC++ 2015-2022 x64 redistributable is a prerequisite on
    the target machine.

.PREREQUISITES
    - Visual Studio 2022 (Build Tools is enough) with the "Desktop development
      with C++" workload. The CMake bundled with VS is used deliberately:
      Kitware CMake 4.x rejects the cmake_minimum_required(VERSION 2.8) in
      libairspy and SoapyAirspy.
    - Git for Windows.
    - SDRplay API 3.x installed (provides sdrplay_api.lib / .dll / headers).
    - 7-Zip (the libusb release asset is a .7z). If missing, the script tries
      `winget install 7zip.7zip`.
    - Internet access for the clones and the libusb download.

.NOTES
    Runtime: about 4-6 minutes cold (clones + full builds), under a minute when
    re-run against an already populated -SourceRoot. The script is idempotent:
    existing checkouts are fetched and re-checked-out to the pinned tag, CMake
    build dirs are reused, and all copies overwrite.

    WORKAROUND 1 - pthreads for libairspy.
      libairspy 1.0.10's airspy.c uses POSIX threads unconditionally, and its
      bundled cmake/modules/FindThreads.cmake forces THREADS_USE_PTHREADS_WIN32
      under MSVC, so `find_package(Threads REQUIRED)` fails without a
      pthreads-win32 install. Rather than shipping pthreadVC3.dll (what the
      PothosSDR build does), this script builds pthreads-win32 3.1.0 as a
      STATIC library and pre-seeds THREADS_PTHREADS_INCLUDE_DIR /
      THREADS_PTHREADS_WIN32_LIBRARY in the CMake cache so FindThreads accepts
      it without searching. Consumers of a static pthreads-win32 must define
      __PTW32_STATIC_LIB, hence the extra CMAKE_C_FLAGS. pthreads-win32 3.1.0
      self-initialises from a .CRT$XCU constructor when built static, so no
      pthread_win32_process_attach_np() call is needed. Result: airspy.dll
      depends only on libusb-1.0.dll, KERNEL32 and the CRT.

    WORKAROUND 2 - SoapySDR module search path.
      On Windows SoapySDR::getRootPath() derives the install root from the
      location of SoapySDR.dll by stripping TWO path components, then looks in
      <root>\lib\SoapySDR\modules0.8. With SoapySDR.dll sitting at the redist
      root that resolves to <parent of redist>\lib\SoapySDR\modules0.8, which
      does not exist. The baked-in fallback (the build machine's install
      prefix) is only used if GetModuleHandleEx fails, i.e. never. So the
      redist relies on the SOAPY_SDR_PLUGIN_PATH environment variable, which
      SoapySDR appends to its search paths. trunkd (or its launcher) must set:
          SOAPY_SDR_PLUGIN_PATH=<redist>\SoapySDR\modules0.8
      This is documented in the generated README.txt as well.

    WORKAROUND 3 - libusb is pinned to 1.0.24, NOT the newest release.
      libairspy 1.0.10's airspy_close() calls libusb_close() BEFORE
      free_transfers(), i.e. it frees transfers that can still be in flight on
      a device whose context has already been torn down. Newer libusb releases
      turn that latent bug into an access violation inside libusb_unref_device
      -> RtlEnterCriticalSection (0xC0000005), killing the process on every
      close of an Airspy.

      Measured on this bundle - same airspy.dll, only libusb-1.0.dll swapped,
      five runs each of

          airspy_rx.exe -r NUL -f 855 -n 4000000 -s <serial>

          libusb 1.0.30   crashed 5/5   deterministic 0xC0000005
          libusb 1.0.25   crashed 3/5   INTERMITTENT - a race, not always fatal
          libusb 1.0.24   crashed 0/5   exit 0 every run

      1.0.24 is the only release observed never to fault, so it is the pin.
      1.0.25 was tried and rejected: it is unsafe in practice even though it
      reads clean in the source (see below).

      Background from libusb/io.c. libusb_free_transfer() gained

          if (itransfer->dev)
                  libusb_unref_device(itransfer->dev);

      in v1.0.26 - not v1.0.28 as the release notes suggest; v1.0.24 and
      v1.0.25 do not have it, v1.0.26 through v1.0.30 do. That explains the
      deterministic failure from 1.0.26 on, but it does NOT explain 1.0.25:
      something else between 1.0.24 and 1.0.25 already makes the close path
      race against transfers still in flight. The pin follows the measurement,
      not the source reading.

      1.0.24 predates the VS2022 binaries in the libusb .7z assets, so the
      build uses its VS2019/MS64/dll files. That is fine: libusb exports a C
      ABI and the VS2019 and VS2022 toolsets share the same VCRUNTIME140 /
      UCRT, so the import library and DLL drop straight into a VS2022 build.
      1.0.24 also puts its header in include\libusb-1.0\ rather than include\,
      so Resolve-LibusbLayout below discovers both the toolset directory and
      the include directory instead of hard-coding either.

      Do NOT bump $LibusbVersion past 1.0.24 without either patching
      libairspy's close ordering or moving to a libairspy release that fixes
      it. Note that swapping only libusb-1.0.dll is not sufficient - airspy.dll
      links against the matching import library, so libairspy must be rebuilt.
#>

[CmdletBinding()]
param(
    # Defaults live under the checkout's own target\ directory (two folders up
    # from this script), so nothing is written outside the repository unless
    # asked. `cargo clean` removes them too; the next run is then a cold one.
    [string] $SourceRoot    = (Join-Path $PSScriptRoot '..\..\target\redist-src'),
    [string] $OutDir        = (Join-Path $PSScriptRoot '..\..\target\redist'),
    [ValidateSet('Release', 'Debug')]
    [string] $Configuration = 'Release'
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# Absolute, normalised paths (no '..' segments, relative ones resolved against
# the current PowerShell location): the summary at the end strips $OutDir off
# each file's FullName, which only works if the two are spelled the same way.
$SourceRoot = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($SourceRoot)
$OutDir     = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutDir)

# ---------------------------------------------------------------------------
# Pinned versions. Change these deliberately; everything else follows.
# ---------------------------------------------------------------------------
# libusb is pinned to 1.0.24 on purpose - 1.0.25 crashes intermittently on close
# with libairspy 1.0.10 and 1.0.26+ crashes every time. See WORKAROUND 3 in the
# header comment, including the measured crash rates, before bumping this.
$LibusbVersion    = '1.0.24'
$LibusbUrl        = "https://github.com/libusb/libusb/releases/download/v$LibusbVersion/libusb-$LibusbVersion.7z"

$Repos = @(
    @{ Name = 'pthread-win32'; Url = 'https://github.com/GerHobbelt/pthread-win32'; Tag = 'version-3.1.0-release' }
    @{ Name = 'SoapySDR';      Url = 'https://github.com/pothosware/SoapySDR';      Tag = 'soapy-sdr-0.8.1' }
    @{ Name = 'airspyone_host';Url = 'https://github.com/airspy/airspyone_host';    Tag = 'v1.0.10' }
    @{ Name = 'SoapyAirspy';   Url = 'https://github.com/pothosware/SoapyAirspy';   Tag = 'soapy-airspy-0.2.0' }
    @{ Name = 'SoapySDRPlay3'; Url = 'https://github.com/pothosware/SoapySDRPlay3'; Tag = 'soapy-sdrplay3-0.5.2' }
)

$TagOf = @{}
foreach ($r in $Repos) { $TagOf[$r.Name] = $r.Tag }

$Generator = 'Visual Studio 17 2022'
$Platform  = 'x64'

# Derived paths.
$DownloadDir      = Join-Path $SourceRoot 'downloads'
$BuildRoot        = Join-Path $SourceRoot 'build'
$LibusbDir        = Join-Path $SourceRoot "libusb-$LibusbVersion"
$InstallSoapy     = Join-Path $SourceRoot 'install'          # SoapySDR + both modules
$InstallAirspy    = Join-Path $SourceRoot 'install-airspy'   # libairspy + tools
$InstallPthread   = Join-Path $SourceRoot 'install-pthread'  # static pthreads-win32
$ModulesOutDir    = Join-Path $OutDir 'SoapySDR\modules0.8'

# ---------------------------------------------------------------------------
# Small helpers
# ---------------------------------------------------------------------------

function Write-Step {
    param([string] $Message)
    Write-Host ''
    Write-Host "==> $Message" -ForegroundColor Cyan
}

# Run a native exe and throw if it fails. PowerShell will not do this for us.
function Invoke-Native {
    param(
        [Parameter(Mandatory)][string] $Exe,
        [Parameter(Mandatory)][string[]] $Arguments
    )
    & $Exe @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "Command failed with exit code ${LASTEXITCODE}: $Exe $($Arguments -join ' ')"
    }
}

function Find-VisualStudio {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (Test-Path $vswhere) {
        $path = & $vswhere -latest -products * `
                    -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
                    -property installationPath
        if ($path) { return ($path | Select-Object -First 1) }
    }
    # Fallback to the known Build Tools location.
    $fallback = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\2022\BuildTools'
    if (Test-Path $fallback) { return $fallback }
    throw 'Visual Studio 2022 with the C++ toolset was not found. Install the "Desktop development with C++" workload.'
}

function Find-Git {
    $cmd = Get-Command git.exe -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    $fallback = Join-Path $env:ProgramFiles 'Git\cmd\git.exe'
    if (Test-Path $fallback) { return $fallback }
    throw 'git.exe was not found. Install Git for Windows.'
}

function Find-SevenZip {
    $candidates = @(
        (Join-Path $env:ProgramFiles '7-Zip\7z.exe')
        (Join-Path ${env:ProgramFiles(x86)} '7-Zip\7z.exe')
        'C:\ProgramData\chocolatey\bin\7z.exe'
    )
    foreach ($c in $candidates) { if (Test-Path $c) { return $c } }
    $cmd = Get-Command 7z.exe -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }

    Write-Host '7-Zip not found; installing via winget...'
    $winget = Get-Command winget.exe -ErrorAction SilentlyContinue
    if (-not $winget) { throw '7-Zip is required to extract the libusb release and winget is unavailable. Install 7-Zip manually.' }
    & $winget.Source install --id 7zip.7zip --exact --silent --accept-package-agreements --accept-source-agreements
    foreach ($c in $candidates) { if (Test-Path $c) { return $c } }
    throw '7-Zip installation did not produce 7z.exe at an expected location.'
}

function Find-SdrplayApiDir {
    $key = 'HKLM:\SOFTWARE\SDRplay\Service\API'
    if (Test-Path $key) {
        $prop = Get-ItemProperty -Path $key -Name 'Install_Dir' -ErrorAction SilentlyContinue
        if ($prop) {
            $dir = $prop.Install_Dir
            if ($dir -and (Test-Path $dir)) { return $dir }
        }
    }
    $fallback = Join-Path $env:ProgramFiles 'SDRplay\API'
    if (Test-Path $fallback) { return $fallback }
    throw 'The SDRplay API installation was not found. Install SDRplay API 3.x.'
}

# sdrplay_api.dll carries no version resource, so read the version the
# installer recorded in the registry instead.
function Get-SdrplayApiVersion {
    $key = 'HKLM:\SOFTWARE\SDRplay\Service\API'
    if (Test-Path $key) {
        $prop = Get-ItemProperty -Path $key -ErrorAction SilentlyContinue
        if ($prop -and $prop.PSObject.Properties['Version']) {
            $v = $prop.Version
            if ($prop.PSObject.Properties['SubVersion']) { $v = "$v.$($prop.SubVersion)" }
            return $v
        }
    }
    return 'unknown'
}

# Clone if absent, otherwise fetch, then check out the pinned tag.
function Sync-Repo {
    param(
        [Parameter(Mandatory)][string] $Git,
        [Parameter(Mandatory)][string] $Url,
        [Parameter(Mandatory)][string] $Path,
        [Parameter(Mandatory)][string] $Tag
    )
    if (-not (Test-Path (Join-Path $Path '.git'))) {
        Write-Host "  cloning $Url -> $Path"
        Invoke-Native $Git @('clone', '--quiet', $Url, $Path)
    }
    # Only hit the network if the tag is not already present locally.
    & $Git -C $Path rev-parse --quiet --verify "refs/tags/$Tag" *> $null
    if ($LASTEXITCODE -ne 0) {
        Write-Host "  fetching tags for $Path"
        Invoke-Native $Git @('-C', $Path, 'fetch', '--quiet', '--tags', 'origin')
    }
    Invoke-Native $Git @('-C', $Path, '-c', 'advice.detachedHead=false', 'checkout', '--quiet', $Tag)
    Write-Host "  $Path @ $Tag ($(& $Git -C $Path rev-parse --short HEAD))"
}

function Invoke-CMakeProject {
    param(
        [Parameter(Mandatory)][string]   $CMake,
        [Parameter(Mandatory)][string]   $SourceDir,
        [Parameter(Mandatory)][string]   $BuildDir,
        [string[]] $ConfigureArgs = @(),
        [string[]] $BuildTargets  = @(),
        [switch]   $Install
    )
    $cfgArgs = @('-S', $SourceDir, '-B', $BuildDir, '-G', $Generator, '-A', $Platform, '-Wno-dev') + $ConfigureArgs
    Invoke-Native $CMake $cfgArgs

    $buildArgs = @('--build', $BuildDir, '--config', $Configuration, '--parallel')
    foreach ($t in $BuildTargets) { $buildArgs += @('--target', $t) }
    Invoke-Native $CMake $buildArgs

    if ($Install) {
        Invoke-Native $CMake @('--install', $BuildDir, '--config', $Configuration)
    }
}

# The libusb release archives are not laid out identically across versions:
#   1.0.27+  VS2022\MS64\dll\libusb-1.0.{dll,lib} and include\libusb.h
#   1.0.25   VS2019\MS64\Release\dll\...          and include\libusb-1.0\libusb.h
#   1.0.24   VS2019\MS64\dll\...                   and include\libusb-1.0\libusb.h
# Discover both rather than hard-coding, so changing $LibusbVersion keeps working.
function Resolve-LibusbLayout {
    param([Parameter(Mandatory)][string] $Root)

    $binDir = $null
    $toolset = $null
    $toolsetDirs = Get-ChildItem -LiteralPath $Root -Directory -Filter 'VS20*' |
                       Sort-Object Name -Descending
    foreach ($vs in $toolsetDirs) {
        foreach ($rel in @('MS64\dll', 'MS64\Release\dll')) {
            $d = Join-Path $vs.FullName $rel
            if ((Test-Path (Join-Path $d 'libusb-1.0.dll')) -and
                (Test-Path (Join-Path $d 'libusb-1.0.lib'))) {
                $binDir  = $d
                $toolset = $vs.Name
                break
            }
        }
        if ($binDir) { break }
    }
    if (-not $binDir) { throw "No MSVC x64 shared-library build was found under $Root." }

    $include = $null
    foreach ($rel in @('include\libusb-1.0', 'include')) {
        $d = Join-Path $Root $rel
        if (Test-Path (Join-Path $d 'libusb.h')) { $include = $d; break }
    }
    if (-not $include) { throw "libusb.h was not found under $Root\include." }

    return [pscustomobject]@{
        Toolset    = $toolset
        Include    = $include
        Dll        = (Join-Path $binDir 'libusb-1.0.dll')
        ImportLib  = (Join-Path $binDir 'libusb-1.0.lib')
    }
}

function Copy-Into {
    param([Parameter(Mandatory)][string] $Source, [Parameter(Mandatory)][string] $Destination)
    if (-not (Test-Path $Source)) { throw "Expected build output is missing: $Source" }
    Copy-Item -LiteralPath $Source -Destination $Destination -Force
}

# ---------------------------------------------------------------------------
# Locate the toolchain
# ---------------------------------------------------------------------------
Write-Step 'Locating toolchain'

$VsRoot = Find-VisualStudio
$CMake  = Join-Path $VsRoot 'Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin\cmake.exe'
if (-not (Test-Path $CMake)) {
    throw "The CMake bundled with Visual Studio was not found at $CMake. Install the CMake component of the C++ workload."
}
$Git         = Find-Git
$SevenZip    = Find-SevenZip
$SdrplayApi  = Find-SdrplayApiDir
$SdrplayInc  = (Join-Path $SdrplayApi 'inc')       -replace '\\', '/'
$SdrplayLib  = (Join-Path $SdrplayApi 'x64\sdrplay_api.lib') -replace '\\', '/'
$SdrplayDll  = Join-Path $SdrplayApi 'x64\sdrplay_api.dll'

Write-Host "  Visual Studio : $VsRoot"
Write-Host "  CMake         : $CMake ($(& $CMake --version | Select-Object -First 1))"
Write-Host "  Git           : $Git"
Write-Host "  7-Zip         : $SevenZip"
Write-Host "  SDRplay API   : $SdrplayApi"
Write-Host "  Source root   : $SourceRoot"
Write-Host "  Output        : $OutDir"
Write-Host "  Configuration : $Configuration"

New-Item -ItemType Directory -Force -Path $SourceRoot, $DownloadDir, $BuildRoot | Out-Null

# ---------------------------------------------------------------------------
# 1. libusb (prebuilt Windows binaries from the upstream release)
# ---------------------------------------------------------------------------
Write-Step "libusb $LibusbVersion (prebuilt MSVC x64 binaries)"

$LibusbArchive = Join-Path $DownloadDir "libusb-$LibusbVersion.7z"

if (-not (Test-Path (Join-Path $LibusbDir 'include'))) {
    if (-not (Test-Path $LibusbArchive)) {
        Write-Host "  downloading $LibusbUrl"
        Invoke-WebRequest -Uri $LibusbUrl -OutFile $LibusbArchive -UseBasicParsing
    }
    Write-Host "  extracting to $LibusbDir"
    Invoke-Native $SevenZip @('x', $LibusbArchive, "-o$LibusbDir", '-y', '-bso0', '-bsp0')
}

$layout        = Resolve-LibusbLayout -Root $LibusbDir
$LibusbDll     = $layout.Dll
$LibusbImpLib  = $layout.ImportLib
$LibusbInclude = $layout.Include
$LibusbToolset = $layout.Toolset
Write-Host "  toolset : $LibusbToolset (x64, shared)"
Write-Host "  include : $LibusbInclude"
Write-Host "  dll     : $LibusbDll"

# ---------------------------------------------------------------------------
# 2. Source checkouts
# ---------------------------------------------------------------------------
Write-Step 'Source checkouts'

$RepoPath = @{}
foreach ($r in $Repos) {
    $path = Join-Path $SourceRoot $r.Name
    Sync-Repo -Git $Git -Url $r.Url -Path $path -Tag $r.Tag
    $RepoPath[$r.Name] = $path
}

# ---------------------------------------------------------------------------
# 3. pthreads-win32 (static) - see WORKAROUND 1 in the header comment
# ---------------------------------------------------------------------------
Write-Step 'pthreads-win32 3.1.0 (static, for libairspy)'

Invoke-CMakeProject -CMake $CMake `
    -SourceDir $RepoPath['pthread-win32'] `
    -BuildDir  (Join-Path $BuildRoot 'pthread-win32') `
    -ConfigureArgs @('-DENABLE_TESTS=OFF') `
    -BuildTargets @('libpthreadVC3')

# The CMake project appends a "d" to the Debug artefact name.
$PthreadLibName  = if ($Configuration -eq 'Debug') { 'libpthreadVC3d.lib' } else { 'libpthreadVC3.lib' }
$PthreadLibBuilt = Join-Path $BuildRoot "pthread-win32\$Configuration\$PthreadLibName"

New-Item -ItemType Directory -Force -Path (Join-Path $InstallPthread 'include'), (Join-Path $InstallPthread 'lib') | Out-Null
foreach ($h in @('pthread.h', 'sched.h', 'semaphore.h', '_ptw32.h')) {
    Copy-Into (Join-Path $RepoPath['pthread-win32'] $h) (Join-Path $InstallPthread 'include')
}
Copy-Into $PthreadLibBuilt (Join-Path $InstallPthread 'lib')
$PthreadInclude = (Join-Path $InstallPthread 'include') -replace '\\', '/'
$PthreadLib     = (Join-Path $InstallPthread "lib\$PthreadLibName") -replace '\\', '/'

# ---------------------------------------------------------------------------
# 4. SoapySDR 0.8.1
# ---------------------------------------------------------------------------
Write-Step 'SoapySDR 0.8.1'

Invoke-CMakeProject -CMake $CMake -Install `
    -SourceDir $RepoPath['SoapySDR'] `
    -BuildDir  (Join-Path $BuildRoot 'SoapySDR') `
    -ConfigureArgs @(
        '-DENABLE_PYTHON=OFF'
        '-DENABLE_PYTHON3=OFF'
        '-DENABLE_APPS=ON'
        '-DENABLE_TESTS=OFF'
        '-DENABLE_DOCS=OFF'
        "-DCMAKE_INSTALL_PREFIX=$InstallSoapy"
    )

$SoapyCMakeDir = (Join-Path $InstallSoapy 'cmake') -replace '\\', '/'

# ---------------------------------------------------------------------------
# 5. libairspy 1.0.10 (+ airspy-tools)
# ---------------------------------------------------------------------------
Write-Step 'libairspy 1.0.10 and airspy-tools'

Invoke-CMakeProject -CMake $CMake -Install `
    -SourceDir $RepoPath['airspyone_host'] `
    -BuildDir  (Join-Path $BuildRoot 'airspy') `
    -ConfigureArgs @(
        "-DLIBUSB_INCLUDE_DIR=$($LibusbInclude -replace '\\','/')"
        "-DLIBUSB_LIBRARIES=$($LibusbImpLib -replace '\\','/')"
        # Pre-seed the pthreads cache entries that the bundled FindThreads.cmake
        # would otherwise search for (and fail to find) under MSVC.
        "-DTHREADS_PTHREADS_INCLUDE_DIR=$PthreadInclude"
        "-DTHREADS_PTHREADS_WIN32_LIBRARY=$PthreadLib"
        # Required when consuming pthreads-win32 as a static library.
        '-DCMAKE_C_FLAGS=/D__PTW32_STATIC_LIB'
        "-DCMAKE_INSTALL_PREFIX=$InstallAirspy"
    )

# libairspy installs both airspy.dll and airspy.lib into <prefix>\bin on Windows.
$AirspyBin     = Join-Path $InstallAirspy 'bin'
$AirspyInclude = (Join-Path $InstallAirspy 'include') -replace '\\', '/'
$AirspyImpLib  = (Join-Path $AirspyBin 'airspy.lib')  -replace '\\', '/'

# ---------------------------------------------------------------------------
# 6. SoapyAirspy 0.2.0
# ---------------------------------------------------------------------------
Write-Step 'SoapyAirspy 0.2.0'

Invoke-CMakeProject -CMake $CMake -Install `
    -SourceDir $RepoPath['SoapyAirspy'] `
    -BuildDir  (Join-Path $BuildRoot 'SoapyAirspy') `
    -ConfigureArgs @(
        "-DSoapySDR_DIR=$SoapyCMakeDir"
        "-DLIBAIRSPY_INCLUDE_DIRS=$AirspyInclude"
        "-DLIBAIRSPY_LIBRARIES=$AirspyImpLib"
        "-DCMAKE_INSTALL_PREFIX=$InstallSoapy"
    )

# ---------------------------------------------------------------------------
# 7. SoapySDRPlay3 0.5.2
# ---------------------------------------------------------------------------
Write-Step 'SoapySDRPlay3 0.5.2'

Invoke-CMakeProject -CMake $CMake -Install `
    -SourceDir $RepoPath['SoapySDRPlay3'] `
    -BuildDir  (Join-Path $BuildRoot 'SoapySDRPlay3') `
    -ConfigureArgs @(
        "-DSoapySDR_DIR=$SoapyCMakeDir"
        "-DLIBSDRPLAY_INCLUDE_DIRS=$SdrplayInc"
        "-DLIBSDRPLAY_LIBRARIES=$SdrplayLib"
        "-DCMAKE_INSTALL_PREFIX=$InstallSoapy"
    )

# ---------------------------------------------------------------------------
# 8. Assemble the redistributable
# ---------------------------------------------------------------------------
Write-Step "Assembling $OutDir"

New-Item -ItemType Directory -Force -Path $OutDir, $ModulesOutDir | Out-Null

Copy-Into (Join-Path $InstallSoapy 'bin\SoapySDR.dll')      $OutDir
Copy-Into (Join-Path $InstallSoapy 'bin\SoapySDRUtil.exe')  $OutDir
Copy-Into (Join-Path $AirspyBin 'airspy.dll')               $OutDir
Copy-Into $LibusbDll                                        $OutDir
# sdrplay_api.dll is always taken from the local SDRplay installation and is
# never vendored into the repository (SDRplay's licence does not permit it).
Copy-Into $SdrplayDll                                       $OutDir

foreach ($tool in @('airspy_info.exe', 'airspy_rx.exe')) {
    $p = Join-Path $AirspyBin $tool
    if (Test-Path $p) { Copy-Into $p $OutDir } else { Write-Warning "$tool was not built; skipping." }
}

Copy-Into (Join-Path $InstallSoapy 'lib\SoapySDR\modules0.8\airspySupport.dll')  $ModulesOutDir
Copy-Into (Join-Path $InstallSoapy 'lib\SoapySDR\modules0.8\sdrPlaySupport.dll') $ModulesOutDir

# Remove stale files from earlier runs that this script no longer produces.
$Expected = @(
    'SoapySDR.dll', 'SoapySDRUtil.exe', 'airspy.dll', 'libusb-1.0.dll',
    'sdrplay_api.dll', 'airspy_info.exe', 'airspy_rx.exe', 'README.txt'
)
Get-ChildItem -LiteralPath $OutDir -File |
    Where-Object { $Expected -notcontains $_.Name } |
    ForEach-Object {
        Write-Host "  removing stale file $($_.Name)"
        Remove-Item -LiteralPath $_.FullName -Force
    }

$ExpectedModules = @('airspySupport.dll', 'sdrPlaySupport.dll')
Get-ChildItem -LiteralPath $ModulesOutDir -File |
    Where-Object { $ExpectedModules -notcontains $_.Name } |
    ForEach-Object {
        Write-Host "  removing stale module $($_.Name)"
        Remove-Item -LiteralPath $_.FullName -Force
    }

# ---------------------------------------------------------------------------
# 9. README.txt
# ---------------------------------------------------------------------------
$sdrplayVersion = Get-SdrplayApiVersion
$readme = @"
trunkd SoapySDR runtime redistributable (Windows x64)
=====================================================

Built on $(Get-Date -Format 'yyyy-MM-dd') by scripts\windows\build-redist.ps1
from the trunkd repository. Everything here is compiled from pinned upstream
sources with MSVC ($Configuration, x64); nothing comes from PothosSDR.

Contents
--------
  SoapySDR.dll                           SoapySDR $($TagOf['SoapySDR'])
  SoapySDRUtil.exe                       SoapySDR diagnostic tool
  airspy.dll                             libairspy $($TagOf['airspyone_host'])
  airspy_info.exe / airspy_rx.exe        airspy-tools (diagnostics)
  libusb-1.0.dll                         libusb $LibusbVersion (upstream $LibusbToolset x64 binary)
  sdrplay_api.dll                        SDRplay API $sdrplayVersion, copied from the
                                         local install at $SdrplayApi
  SoapySDR\modules0.8\airspySupport.dll  SoapyAirspy $($TagOf['SoapyAirspy'])
  SoapySDR\modules0.8\sdrPlaySupport.dll SoapySDRPlay3 $($TagOf['SoapySDRPlay3'])
  (airspy.dll also statically contains pthreads-win32 $($TagOf['pthread-win32']))

trunkd.exe is dropped directly into this folder, next to SoapySDR.dll.

Prerequisites on the target machine
-----------------------------------
1. Microsoft Visual C++ 2015-2022 x64 Redistributable:

       winget install Microsoft.VCRedist.2015+.x64

   This is deliberately NOT bundled here. The SoapySDR modules pass
   std::string and other C++ standard library objects across DLL boundaries,
   so SoapySDR.dll, the modules and trunkd.exe must all use the same shared
   CRT. Copying vcruntime140.dll / msvcp140.dll into this folder would risk
   loading a second, private copy of the CRT.

2. SDRplay API 3.x service installed and running (for the RSPdx). The DLL
   here is the client library; the background service is a separate install.

3. Airspy devices need the WinUSB driver (Zadig) as usual.

libusb version pin (do not "upgrade" this)
------------------------------------------
libusb-1.0.dll here is $LibusbVersion, deliberately NOT the newest release.

libairspy 1.0.10 closes the Airspy before freeing its USB transfers, so
transfers can still be in flight when their device goes away. Newer libusb
releases turn that into an access violation in libusb_unref_device ->
RtlEnterCriticalSection (0xC0000005) on every airspy_close(). Measured with

    airspy_rx.exe -r NUL -f 855 -n 4000000 -s <serial>

five runs each, same airspy.dll, only libusb-1.0.dll swapped:

    libusb 1.0.30   crashed 5/5   deterministic
    libusb 1.0.25   crashed 3/5   intermittent - a race
    libusb 1.0.24   crashed 0/5   exit 0 every run

1.0.24 is the only release observed never to fault, so it is what ships here;
1.0.25 was tried and rejected. 1.0.24 predates the VS2022 assets in the libusb
release archives, so these are its $LibusbToolset x64 binaries - same C ABI and same
VCRUNTIME140/UCRT, so they are interchangeable with a VS2022 build.

Dropping a newer libusb-1.0.dll into this folder will reintroduce the crash,
and swapping the DLL alone is not enough anyway - airspy.dll is linked against
the matching import library, so libairspy has to be rebuilt with it.

Module search path (IMPORTANT)
------------------------------
SoapySDR locates its plugin modules relative to SoapySDR.dll by stripping two
path components and appending lib\SoapySDR\modules0.8. Because SoapySDR.dll
lives at the root of this folder, that default path does not resolve here, so
the modules must be pointed at explicitly:

    set SOAPY_SDR_PLUGIN_PATH=%~dp0SoapySDR\modules0.8

or, in PowerShell:

    `$env:SOAPY_SDR_PLUGIN_PATH = "<this folder>\SoapySDR\modules0.8"

Verify with:

    SoapySDRUtil.exe --info

which must list both modules with no load errors, and:

    SoapySDRUtil.exe --find="driver=sdrplay"
    SoapySDRUtil.exe --find="driver=airspy"

How this was built
------------------
Visual Studio 2022 C++ toolset with the CMake bundled with Visual Studio
(Kitware CMake 4.x refuses the cmake_minimum_required(VERSION 2.8) used by
libairspy and SoapyAirspy). libairspy needs POSIX threads on Windows; the
build links pthreads-win32 3.1.0 statically into airspy.dll, so no
pthreadVC3.dll is required here. See the header comment of
scripts\windows\build-redist.ps1 for the full recipe and the pinned tags.
"@
Set-Content -LiteralPath (Join-Path $OutDir 'README.txt') -Value $readme -Encoding ASCII

# ---------------------------------------------------------------------------
# 10. Manifest
# ---------------------------------------------------------------------------
Write-Step 'Manifest'

Write-Host ''
Write-Host 'Pinned versions:'
Write-Host ("  {0,-16} {1}" -f 'libusb', "$LibusbVersion ($LibusbToolset x64 binaries; pinned - see WORKAROUND 3)")
foreach ($r in $Repos) {
    $sha = & $Git -C $RepoPath[$r.Name] rev-parse --short HEAD
    Write-Host ("  {0,-16} {1} ({2})" -f $r.Name, $r.Tag, $sha)
}
Write-Host ("  {0,-16} {1}" -f 'SDRplay API', "$sdrplayVersion (from $SdrplayApi)")

Write-Host ''
Write-Host "Contents of ${OutDir}:"
Get-ChildItem -LiteralPath $OutDir -Recurse -File |
    Sort-Object FullName |
    ForEach-Object {
        Write-Host ("  {0,10}  {1}" -f $_.Length, $_.FullName.Substring($OutDir.Length + 1))
    }

Write-Host ''
Write-Host 'Next: set SOAPY_SDR_PLUGIN_PATH and run SoapySDRUtil.exe --info from that folder.' -ForegroundColor Green
