<#
.SYNOPSIS
  Run the DoodleIQ provider agent at logon and keep it alive (Windows).

.DESCRIPTION
  A plain console binary cannot be a real Windows Service without a wrapper, so
  this registers a Scheduled Task that starts `doodleiq run` at logon, runs it
  hidden, and restarts it if it exits. Run `doodleiq configure` and
  `doodleiq pair` once (as the same user) before installing.

.EXAMPLE
  # install (from an elevated PowerShell, in the folder containing doodleiq.exe)
  .\install-windows-service.ps1

  # remove
  .\install-windows-service.ps1 -Uninstall
#>
param(
    [string]$ExePath = (Join-Path $PSScriptRoot "..\doodleiq.exe" | Resolve-Path -ErrorAction SilentlyContinue),
    [switch]$Uninstall
)

$TaskName = "DoodleIQ Provider"

if ($Uninstall) {
    Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false -ErrorAction SilentlyContinue
    Write-Host "Removed scheduled task '$TaskName'."
    return
}

if (-not $ExePath -or -not (Test-Path $ExePath)) {
    throw "doodleiq.exe not found. Pass -ExePath 'C:\path\to\doodleiq.exe'."
}
$ExePath = (Resolve-Path $ExePath).Path

$action  = New-ScheduledTaskAction -Execute $ExePath -Argument "run"
$trigger = New-ScheduledTaskTrigger -AtLogOn
$settings = New-ScheduledTaskSettingsSet `
    -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) `
    -ExecutionTimeLimit (New-TimeSpan -Seconds 0) `
    -MultipleInstances IgnoreNew
$principal = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive -RunLevel Limited

Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger `
    -Settings $settings -Principal $principal -Force | Out-Null

Write-Host "Installed scheduled task '$TaskName'. Starting it now..."
Start-ScheduledTask -TaskName $TaskName
