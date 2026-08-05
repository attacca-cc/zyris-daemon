# zyrisd install script — native Windows (PowerShell 5.1+).
#
#   irm https://github.com/attacca-cc/zyris-daemon/releases/latest/download/install.ps1 | iex
#
# **Installs the binary only.** The human enrolls afterwards (same contract as Linux install.sh).
# Options come from env vars: ZYRISD_BASE_URL, ZYRISD_INSTALL_DIR, ZYRISD_NO_AUTOSTART=1
$ErrorActionPreference = 'Stop'

$BaseUrl = if ($env:ZYRISD_BASE_URL) { $env:ZYRISD_BASE_URL } else { 'https://github.com/attacca-cc/zyris-daemon/releases/latest/download' }
$ZipName = 'zyrisd-x86_64-windows.zip'
$InstallDir = if ($env:ZYRISD_INSTALL_DIR) { $env:ZYRISD_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'zyrisd' }
$Tmp = Join-Path $env:TEMP ('zyrisd-install-' + [guid]::NewGuid().ToString('N'))

function Fail([string]$msg) { Write-Error "Error: $msg" }

try {
  New-Item -ItemType Directory -Force -Path $Tmp | Out-Null
  $zip = Join-Path $Tmp $ZipName
  Write-Host "Downloading: $BaseUrl/$ZipName"
  Invoke-WebRequest -Uri "$BaseUrl/$ZipName" -OutFile $zip
  Invoke-WebRequest -Uri "$BaseUrl/SHA256SUMS" -OutFile (Join-Path $Tmp 'SHA256SUMS')

  # Same host serves the checksum and the zip, so this catches transfer damage only. No signing yet.
  $line = Get-Content (Join-Path $Tmp 'SHA256SUMS') | Where-Object { $_ -match (' ' + [regex]::Escape($ZipName) + '$') }
  if (-not $line) { Fail "No entry for $ZipName in SHA256SUMS" }
  $expected = ($line -split '\s+')[0]
  $actual = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLower()
  if ($actual -ne $expected.ToLower()) { Fail "Checksum mismatch (expected $expected, got $actual)" }

  Expand-Archive -Path $zip -DestinationPath $Tmp -Force
  New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
  Copy-Item (Join-Path $Tmp 'zyrisd.exe') (Join-Path $InstallDir 'zyrisd.exe') -Force

  # PATH (user environment)
  $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
  if ($userPath -notlike "*$InstallDir*") {
    [Environment]::SetEnvironmentVariable('Path', ($userPath.TrimEnd(';') + ';' + $InstallDir), 'User')
    Write-Host "Added $InstallDir to PATH (takes effect in new shells)."
  }

  # Auto-connect at boot (HKCU Run key → zyrisd run). ZYRISD_NO_AUTOSTART=1 turns it off.
  if ($env:ZYRISD_NO_AUTOSTART -ne '1') {
    $runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
    New-Item -Path $runKey -Force | Out-Null
    Set-ItemProperty -Path $runKey -Name 'zyrisd' -Value ('"' + (Join-Path $InstallDir 'zyrisd.exe') + '" run')
    Write-Host 'Auto-connect at boot is on (re-run with ZYRISD_NO_AUTOSTART=1 to turn it off).'
  }

  Write-Host ''
  Write-Host "Installed zyrisd at $InstallDir\zyrisd.exe."
  Write-Host ''
  Write-Host 'One step left (in a new shell):'
  Write-Host ''
  Write-Host '  zyrisd enroll     enroll this computer with your Attacca account'
  Write-Host ''
}
finally {
  Remove-Item -Recurse -Force -Path $Tmp -ErrorAction SilentlyContinue
}
