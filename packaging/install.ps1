<#
.SYNOPSIS
  Install the DoodleIQ provider agent on Windows.

  irm https://get.doodleiq.com/install.ps1 | iex

.PARAMETER Version
  Release to install (default: latest).
.PARAMETER BaseUrl
  Where release archives live (default: baked in at release time; falls back to
  GitHub releases).
.PARAMETER BinDir
  Install directory (default: %LOCALAPPDATA%\Programs\DoodleIQ).
#>
param(
    [string]$Version = "latest",
    [string]$BaseUrl = "",
    [string]$BinDir  = (Join-Path $env:LOCALAPPDATA "Programs\DoodleIQ")
)

$ErrorActionPreference = "Stop"

# Substituted by release-local.sh at publish time; @@...@@ fallbacks apply from the repo.
$repo = "@@REPO@@"; if ($repo -like "*@@*") { $repo = "rahulbats/doodleiq-agent" }
if (-not $BaseUrl) { $BaseUrl = "@@BASE_URL@@"; if ($BaseUrl -like "*@@*") { $BaseUrl = "" } }

$archive = "doodleiq-windows-x86_64.zip"
if ($BaseUrl) {
    $url = "$($BaseUrl.TrimEnd('/'))/$Version/$archive"
} elseif ($Version -eq "latest") {
    $url = "https://github.com/$repo/releases/latest/download/$archive"
} else {
    $url = "https://github.com/$repo/releases/download/$Version/$archive"
}

New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
$tmp = New-Item -ItemType Directory -Path (Join-Path $env:TEMP ([guid]::NewGuid()))
try {
    Write-Host "Downloading $url"
    $zip = Join-Path $tmp "a.zip"
    Invoke-WebRequest -Uri $url -OutFile $zip
    Expand-Archive -Path $zip -DestinationPath $tmp -Force
    Copy-Item (Join-Path $tmp "doodleiq-windows-x86_64\doodleiq.exe") (Join-Path $BinDir "doodleiq.exe") -Force
} finally {
    Remove-Item -Recurse -Force $tmp
}

$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($userPath -notlike "*$BinDir*") {
    [Environment]::SetEnvironmentVariable("Path", "$userPath;$BinDir", "User")
    Write-Host "Added $BinDir to your PATH (restart the terminal)."
}

Write-Host "Installed doodleiq to $BinDir\doodleiq.exe"
Write-Host ""
Write-Host "Next:"
Write-Host "  doodleiq configure    # point at your model runtime (Ollama/LM Studio/...)"
Write-Host "  doodleiq pair         # link this machine to your DoodleIQ account"
Write-Host "  doodleiq run          # start serving"
Write-Host "  # then: packaging\install-windows-service.ps1  to keep it running"
