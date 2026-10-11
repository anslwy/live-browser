# Live Browser installer (Windows x86_64, PowerShell 5.1+).
#
# Run from an elevated-or-normal PowerShell:
#
#   irm https://raw.githubusercontent.com/anslwy/live-browser/main/install.ps1 | iex
#
# What it does by default (fast path):
#   1. checks for Google Chrome (installs via winget if missing)
#   2. downloads the latest prebuilt `lb` release (lb.exe + web UI)
#   3. installs to %LOCALAPPDATA%\live-browser (override with -Prefix)
#
# With -FromSource it installs the Rust + Node toolchains if missing,
# clones the repo and builds everything locally instead:
#
#   irm .../install.ps1 -OutFile install.ps1; .\install.ps1 -FromSource
#
[CmdletBinding()]
param(
  [string]$Prefix = (Join-Path $env:LOCALAPPDATA "live-browser"),
  [switch]$FromSource
)

$ErrorActionPreference = "Stop"
$Repo = "anslwy/live-browser"

function Write-Step([string]$Text) { Write-Host "==> $Text" -ForegroundColor Green }

function Get-LatestTag {
  $api = "https://api.github.com/repos/$Repo/releases/latest"
  try {
    $rel = Invoke-RestMethod -Uri $api -UseBasicParsing
    return $rel.tag_name
  } catch {
    throw "could not determine latest release (network? rate limit?): $_"
  }
}

function Test-Browser {
  $candidates = @(
    "${env:ProgramFiles}\Google\Chrome\Application\chrome.exe",
    "${env:ProgramFiles(x86)}\Google\Chrome\Application\chrome.exe",
    (Join-Path $env:LOCALAPPDATA "Google\Chrome\Application\chrome.exe")
  )
  foreach ($c in $candidates) { if (Test-Path $c) { return $c } }
  if (Get-Command chrome -ErrorAction SilentlyContinue) { return "chrome (PATH)" }
  return $null
}

function Install-Browser {
  $found = Test-Browser
  if ($found) { Write-Step "browser already installed ($found)"; return }
  Write-Step "installing Google Chrome…"
  $winget = Get-Command winget -ErrorAction SilentlyContinue
  if ($winget) {
    winget install --exact --silent Google.Chrome --accept-package-agreements --accept-source-agreements
  } else {
    throw "no browser found — install Google Chrome from https://www.google.com/chrome/, then re-run"
  }
}

function Install-Release {
  $tag = Get-LatestTag
  $asset = "live-browser-$tag-windows-x86_64.zip"
  $url = "https://github.com/$Repo/releases/download/$tag/$asset"
  Write-Step "installing $asset…"
  $tmp = Join-Path ([IO.Path]::GetTempPath()) ("lb-" + [Guid]::NewGuid().ToString("N"))
  New-Item -ItemType Directory -Path $tmp | Out-Null
  try {
    $zip = Join-Path $tmp "pkg.zip"
    try {
      Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
    } catch {
      throw "no prebuilt binary for windows-x86_64 yet ($asset): $_ — or re-run with -FromSource"
    }
    $extract = Join-Path $tmp "pkg"
    Expand-Archive -Path $zip -DestinationPath $extract -Force
    $inner = Get-ChildItem $extract -Directory | Where-Object { $_.Name -like "live-browser-*" } | Select-Object -First 1
    if (-not $inner) { throw "unexpected archive layout in $asset" }
    if (Test-Path $Prefix) { Remove-Item -Recurse -Force $Prefix }
    New-Item -ItemType Directory -Path $Prefix -Force | Out-Null
    Copy-Item (Join-Path $inner.FullName "*") $Prefix -Recurse -Force
  } finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
  }
}

function Install-FromSource {
  if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Step "installing rust (rustup)…"
    $tmp = Join-Path ([IO.Path]::GetTempPath()) ("rustup-" + [Guid]::NewGuid().ToString("N") + ".exe")
    Invoke-WebRequest -Uri "https://win.rustup.rs/x86_64" -OutFile $tmp -UseBasicParsing
    & $tmp -y --default-toolchain stable --profile minimal
    Remove-Item $tmp -Force -ErrorAction SilentlyContinue
    $cargoBin = Join-Path $env:USERPROFILE ".cargo\bin"
    $env:PATH = "$cargoBin;$env:PATH"
  } else {
    Write-Step "rust already installed ($(cargo --version))"
  }
  $node = Get-Command node -ErrorAction SilentlyContinue
  if ($node) {
    $major = (& node --version) -replace '^v(\d+)\..*', '$1'
    if ([int]$major -ge 20) { Write-Step "node already installed ($(node --version))"; $node = $true }
    else { $node = $null }
  }
  if (-not $node) {
    Write-Step "installing node 22 (winget)…"
    $winget = Get-Command winget -ErrorAction SilentlyContinue
    if (-not $winget) { throw "need Node.js 20+ — install from https://nodejs.org/, then re-run" }
    winget install --exact --silent OpenJS.NodeJS.LTS.22 --accept-package-agreements --accept-source-agreements
    # Refresh PATH for this session.
    $env:PATH = [Environment]::GetEnvironmentVariable("PATH", "Machine") + ";" + [Environment]::GetEnvironmentVariable("PATH", "User")
  }
  if (-not (Get-Command git -ErrorAction SilentlyContinue)) { throw "need git — install from https://git-scm.com/, then re-run" }
  $work = Join-Path ([IO.Path]::GetTempPath()) ("lb-src-" + [Guid]::NewGuid().ToString("N"))
  try {
    Write-Step "cloning $Repo…"
    git clone --depth 1 "https://github.com/$Repo.git" "$work"
    Push-Location "$work\web"
    try { npm ci; npm run build } finally { Pop-Location }
    Push-Location $work
    try { cargo build --release } finally { Pop-Location }
    Write-Step "installing to $Prefix…"
    if (Test-Path $Prefix) { Remove-Item -Recurse -Force $Prefix }
    New-Item -ItemType Directory -Path (Join-Path $Prefix "web") -Force | Out-Null
    Copy-Item (Join-Path $work "target\release\lb.exe") $Prefix -Force
    Copy-Item (Join-Path $work "web\dist") (Join-Path $Prefix "web\dist") -Recurse -Force
  } finally {
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
  }
}

function Add-ToUserPath([string]$Dir) {
  $userPath = [Environment]::GetEnvironmentVariable("PATH", "User")
  if (($userPath -split ";") -notcontains $Dir) {
    [Environment]::SetEnvironmentVariable("PATH", "$userPath;$Dir", "User")
    Write-Host "NOTE: added $Dir to your user PATH — restart the terminal to use it."
  }
  if (($env:PATH -split ";") -notcontains $Dir) { $env:PATH += ";$Dir" }
}

Install-Browser
if ($FromSource) { Install-FromSource } else { Install-Release }
Add-ToUserPath $Prefix

Write-Step "done!"
Write-Host ""
Write-Host "  lb daemon --port 8765          # start browser + web UI"
Write-Host "  lb open https://example.com    # drive it"
