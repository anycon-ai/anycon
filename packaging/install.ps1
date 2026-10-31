# Install the anycon CLI (daemon + client in one binary) on Windows from GitHub Releases.
#
#   irm https://github.com/anycon-ai/anycon/releases/latest/download/install.ps1 | iex
#
# Environment (set it before running, e.g. `$env:ANYCON_VERSION = '0.1.0'`):
#   ANYCON_VERSION       version to install, e.g. 0.1.0 (default: the release this script came from)
#   ANYCON_INSTALL_DIR   where to put the binaries (default: %LOCALAPPDATA%\Programs\anycon\bin)
#
# Supported: Windows 10 1809 / Windows Server 2019 or newer, x64 (Windows on Arm runs the x64 build under emulation).
# The script downloads anycon-x86_64-pc-windows-msvc.zip and its .sha256 from the release, verifies the checksum,
# installs anycon.exe and anyconw.exe (the console-less launcher used by `anycon service install`), puts LICENSE and
# THIRD-PARTY-LICENSES.txt under <install dir>\..\doc, and adds the install directory to your user PATH.
# It never needs administrator rights.
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$Repo = 'anycon-ai/anycon'
$Target = 'x86_64-pc-windows-msvc'
# The copy attached to a GitHub Release has its version filled in by the release workflow;
# this copy from the repository installs the latest release.
$DefaultVersion = ''

function Say([string]$msg) { Write-Host "anycon-install: $msg" }
function Fail([string]$msg) { throw "anycon-install: error: $msg" }

if ([Environment]::OSVersion.Platform -ne 'Win32NT') { Fail 'this script is for Windows; on macOS / Linux use install.sh' }
if (-not [Environment]::Is64BitOperatingSystem) { Fail 'a 64-bit Windows is required' }
if ([Environment]::OSVersion.Version.Build -lt 17763) { Fail 'Windows 10 1809 (build 17763) or newer is required' }

$version = if ($env:ANYCON_VERSION) { $env:ANYCON_VERSION.TrimStart('v') } else { $DefaultVersion }
$base = if ($version) { "https://github.com/$Repo/releases/download/v$version" } else { "https://github.com/$Repo/releases/latest/download" }
$installDir = if ($env:ANYCON_INSTALL_DIR) { $env:ANYCON_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\anycon\bin' }

$name = "anycon-$Target"
$tmp = Join-Path ([IO.Path]::GetTempPath()) ("anycon-install-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    $zip = Join-Path $tmp "$name.zip"
    Say "downloading $base/$name.zip"
    Invoke-WebRequest -UseBasicParsing -Uri "$base/$name.zip" -OutFile $zip
    Invoke-WebRequest -UseBasicParsing -Uri "$base/$name.zip.sha256" -OutFile "$zip.sha256"
    $want = ((Get-Content -Raw "$zip.sha256").Trim() -split '\s+')[0].ToLowerInvariant()
    $got = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLowerInvariant()
    if ($want -ne $got) { Fail "checksum mismatch for $name.zip (expected $want, got $got)" }

    Expand-Archive -Path $zip -DestinationPath $tmp
    $src = Join-Path $tmp $name
    New-Item -ItemType Directory -Force -Path $installDir | Out-Null
    foreach ($exe in 'anycon.exe', 'anyconw.exe') {
        $dest = Join-Path $installDir $exe
        if (Test-Path $dest) {
            # A running executable cannot be overwritten, but it can be renamed: move the old one aside first
            $old = "$dest.old"
            Remove-Item -Force -ErrorAction SilentlyContinue $old
            Move-Item -Force $dest $old
        }
        Copy-Item (Join-Path $src $exe) $dest
        Remove-Item -Force -ErrorAction SilentlyContinue "$dest.old"
    }
    $docDir = Join-Path (Split-Path -Parent $installDir) 'doc'
    New-Item -ItemType Directory -Force -Path $docDir | Out-Null
    foreach ($f in 'LICENSE', 'THIRD-PARTY-LICENSES.txt') { Copy-Item (Join-Path $src $f) $docDir }

    $installed = & (Join-Path $installDir 'anycon.exe') --version
    Say "installed $installed to $installDir"

    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $entries = @($userPath -split ';' | Where-Object { $_ })
    if ($entries -notcontains $installDir) {
        [Environment]::SetEnvironmentVariable('Path', (($entries + $installDir) -join ';'), 'User')
        $env:Path = "$env:Path;$installDir"
        Say "added $installDir to your user PATH (open a new terminal for other windows to see it)"
    }
    Say 'next: anycon init --hub <your hub URL>, then anycon service install'
}
finally {
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $tmp
}
