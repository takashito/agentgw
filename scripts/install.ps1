# Install agentgw on Windows as a machine: check psmux and Claude Code -> a private PowerShell 7 for
# agentgw -> fetch the binary -> connect to the gateway -> register the scheduled task and start it.
#
#   powershell -ExecutionPolicy Bypass -File install.ps1 [-From <agentgw.exe>] [-Name <name>] [-ConnectionFile <file>]
#
# Windows joins as a **machine** only; the gateway stays elsewhere. On the gateway, make the
# connection string first:
#   agentgw add-machine --print <name>
# and paste it when this script asks.
#
# **This is for the first install.** Later versions arrive with `update`, like every other machine.
#
# Run it in an **administrator** PowerShell (Windows PowerShell 5.1 or PowerShell 7): the scheduled task runs as you whether or not you are
# logged in, and Windows asks for your password once to allow that (Get-Credential hands it to Task
# Scheduler; agentgw never sees it).
#
# Environment:
#   AGENTGW_STATE_DIR  state directory (default: %LOCALAPPDATA%\agentgw)
#   AGENTGW_REPO       repository to fetch releases from (default: takashito/agentgw)
#   AGENTGW_TAG        release tag to fetch (default: the latest release)
param(
    [string]$From,
    [string]$Name,
    # A file holding the connection string, instead of pasting it (deleted once it is used)
    [string]$ConnectionFile
)
$ErrorActionPreference = 'Stop'
# Runs in the Windows PowerShell 5.1 every Windows has, as well as in PowerShell 7. Hence: ASCII only
# (5.1 reads a script without a BOM as the ANSI code page), TLS 1.2 by hand, -UseBasicParsing
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

function Say([string]$s) { Write-Host $s }
function Ok([string]$s) { Write-Host "  ok $s" }
function Fail([string]$s) { Write-Host "  x $s" -ForegroundColor Red; exit 1 }

$repo = if ($env:AGENTGW_REPO) { $env:AGENTGW_REPO } else { 'takashito/agentgw' }
$stateDir = if ($env:AGENTGW_STATE_DIR) { $env:AGENTGW_STATE_DIR } else { Join-Path $env:LOCALAPPDATA 'agentgw' }
$bin = Join-Path $HOME '.local\bin'
$exe = Join-Path $bin 'agentgw.exe'
# The machine's CPU, not this shell's (an emulated x64 shell sees AMD64 in PROCESSOR_ARCHITECTURE)
$cpu = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
$arch = if ($cpu -eq 'ARM64') { 'aarch64' } else { 'x86_64' }
$triple = "$arch-pc-windows-gnullvm"

$admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $admin) { Fail 'Open PowerShell with "Run as administrator" and run this again.' }

Say "`nagentgw install - Windows ($triple)`n"

# 1. psmux (tmux for Windows) - agentgw keeps its agents in it
$tmux = Join-Path $env:LOCALAPPDATA 'Microsoft\WinGet\Links\tmux.exe'
if (-not (Test-Path $tmux)) {
    Say '  - installing psmux with winget'
    winget install --id marlocarlo.psmux --exact --accept-source-agreements --accept-package-agreements | Out-Null
    if (-not (Test-Path $tmux)) { Fail "psmux did not install ($tmux is missing)" }
}
Ok "psmux: $((& $tmux -V | Select-Object -First 1))"

# 2. Claude Code, the standalone CLI (not the one inside the desktop app)
$claude = Join-Path $bin 'claude.exe'
if (-not (Test-Path $claude)) {
    Fail "Claude Code is not installed at $claude. Install it with: irm https://claude.ai/install.ps1 | iex - then run ``claude`` once to sign in, and run this again."
}
Ok "Claude Code: $((& $claude --version 2>&1 | Out-String).Trim())"
if (-not (Test-Path (Join-Path $HOME '.claude\.credentials.json'))) {
    Say '  ! Claude Code has no sign-in yet. Run `claude` once and sign in before the first agent starts.'
}

# 3. A PowerShell 7 of agentgw's own. psmux starts every window through `pwsh`, and the Store build
#    cannot start while you are logged out. Only agentgw's psmux sees this one; your PATH is untouched.
$pwDir = Join-Path $env:LOCALAPPDATA 'agentgw\pwsh'
if (-not (Test-Path (Join-Path $pwDir 'pwsh.exe'))) {
    $psArch = if ($arch -eq 'aarch64') { 'arm64' } else { 'x64' }
    $rel = Invoke-RestMethod 'https://api.github.com/repos/PowerShell/PowerShell/releases/latest'
    $asset = $rel.assets | Where-Object { $_.name -like "PowerShell-*-win-$psArch.zip" } | Select-Object -First 1
    if (-not $asset) { Fail "no PowerShell ZIP for win-$psArch in the latest release" }
    Say "  - downloading $($asset.name) for agentgw"
    $zip = Join-Path $env:TEMP $asset.name
    Invoke-WebRequest $asset.browser_download_url -OutFile $zip -UseBasicParsing
    New-Item -ItemType Directory -Force $pwDir | Out-Null
    Expand-Archive $zip -DestinationPath $pwDir -Force
    Remove-Item $zip
}
Ok "PowerShell for agentgw: $((& (Join-Path $pwDir 'pwsh.exe') -NoProfile -Command '$PSVersionTable.PSVersion.ToString()' | Out-String).Trim()) ($pwDir)"

# 4. The agentgw binary
New-Item -ItemType Directory -Force $bin | Out-Null
$new = Join-Path $bin 'agentgw.new.exe'
if ($From) {
    Copy-Item $From $new -Force
    Ok "using $From"
} else {
    $tag = $env:AGENTGW_TAG
    if (-not $tag) { $tag = (Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest").tag_name }
    $url = "https://github.com/$repo/releases/download/$tag/agentgw-$triple"
    Say "  - downloading agentgw $tag"
    Invoke-WebRequest $url -OutFile $new -UseBasicParsing
    $want = (((New-Object Net.WebClient).DownloadString("$url.sha256")) -split '\s+')[0].Trim().ToLower()
    $got = (Get-FileHash $new -Algorithm SHA256).Hash.ToLower()
    if ($want -ne $got) { Remove-Item $new; Fail "checksum mismatch (expected $want, got $got)" }
    Ok "agentgw $tag (checksum matches)"
}
# A running agentgw.exe cannot be overwritten, but it can be renamed
if (Test-Path $exe) {
    Remove-Item (Join-Path $bin 'agentgw.prev.exe') -Force -ErrorAction SilentlyContinue
    Move-Item $exe (Join-Path $bin 'agentgw.prev.exe') -Force
}
Move-Item $new $exe -Force
Ok "installed $exe - $((& $exe --build | Out-String).Trim())"

# 5. Connect to the gateway (once)
$env:AGENTGW_STATE_DIR = $stateDir
$envFile = Join-Path $stateDir '.env'
$linked = (Test-Path $envFile) -and (Select-String -Path $envFile -Pattern '^AGENTGW_BRIDGE_ROLE=machine' -Quiet)
if ($linked) {
    Ok 'already connected to a gateway (leaving .env as it is)'
} else {
    if (-not $Name) {
        $default = $env:COMPUTERNAME.ToLower()
        $Name = Read-Host "  Name of this machine as the gateway will know it [$default]"
        if (-not $Name) { $Name = $default }
    }
    Say "  On the gateway, run:  agentgw add-machine --print $Name"
    if ($ConnectionFile) {
        $conn = (Get-Content -Raw $ConnectionFile).Trim()
    } else {
        $secure = Read-Host '  Paste the connection string' -AsSecureString
        $conn = [Runtime.InteropServices.Marshal]::PtrToStringBSTR([Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure))
    }
    if (-not $conn) { Fail 'no connection string given' }
    $conn | & $exe link --name $Name -
    if ($LASTEXITCODE -ne 0) { Fail 'agentgw link failed' }
    Ok "connected as $Name"
    if ($ConnectionFile) { Remove-Item $ConnectionFile -Force }
}

# 6. The scheduled task (asks for your Windows password) - then started
& $exe install
if ($LASTEXITCODE -ne 0) { Fail 'agentgw install failed' }
Start-Sleep 5
& $exe status
Say "`nDone. On the gateway, ``agentgw status`` lists this machine once it has connected."
