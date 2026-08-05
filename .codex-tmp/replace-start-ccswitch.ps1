[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$SourceExecutable,

    [Parameter(Mandatory = $true)]
    [string]$InstalledExecutable,

    [Parameter(Mandatory = $true)]
    [string]$BackupExecutable,

    [int]$RemoteDebuggingPort = 0
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$sourcePath = [System.IO.Path]::GetFullPath($SourceExecutable)
$installedPath = [System.IO.Path]::GetFullPath($InstalledExecutable)
$backupPath = [System.IO.Path]::GetFullPath($BackupExecutable)
$sourceHash = (Get-FileHash -LiteralPath $sourcePath -Algorithm SHA256).Hash
$installedHash = (Get-FileHash -LiteralPath $installedPath -Algorithm SHA256).Hash
$backupHash = (Get-FileHash -LiteralPath $backupPath -Algorithm SHA256).Hash

if (-not [string]::Equals($installedHash, $backupHash, [StringComparison]::OrdinalIgnoreCase)) {
    throw "Installed CC Switch no longer matches the verified backup."
}

$existing = Get-CimInstance Win32_Process -Filter "Name='cc-switch.exe'" -ErrorAction SilentlyContinue
if ($null -ne $existing) {
    throw "CC Switch is running; refusing an unlocked replacement attempt."
}

$replacementPath = "$installedPath.replacement-$PID"
$replaced = $false
for ($attempt = 1; $attempt -le 30; $attempt++) {
    try {
        Copy-Item -LiteralPath $sourcePath -Destination $replacementPath -Force
        if (-not [string]::Equals(
            (Get-FileHash -LiteralPath $replacementPath -Algorithm SHA256).Hash,
            $sourceHash,
            [StringComparison]::OrdinalIgnoreCase
        )) {
            throw "Replacement executable hash verification failed."
        }
        Move-Item -LiteralPath $replacementPath -Destination $installedPath -Force
        $replaced = $true
        break
    } catch [System.IO.IOException] {
        Remove-Item -LiteralPath $replacementPath -Force -ErrorAction SilentlyContinue
        Start-Sleep -Seconds 1
    }
}

if (-not $replaced) {
    Start-Process -FilePath $installedPath -WindowStyle Hidden | Out-Null
    throw "Installed executable remained locked for 30 seconds; original CC Switch was restarted."
}

if (-not [string]::Equals(
    (Get-FileHash -LiteralPath $installedPath -Algorithm SHA256).Hash,
    $sourceHash,
    [StringComparison]::OrdinalIgnoreCase
)) {
    Copy-Item -LiteralPath $backupPath -Destination $installedPath -Force
    Start-Process -FilePath $installedPath -WindowStyle Hidden | Out-Null
    throw "Installed executable hash verification failed; original CC Switch was restored."
}

$previousBrowserArguments = $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS
try {
    if ($RemoteDebuggingPort -gt 0) {
        $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = "--remote-debugging-port=$RemoteDebuggingPort"
    } else {
        Remove-Item Env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS -ErrorAction SilentlyContinue
    }
    $newProcess = Start-Process -FilePath $installedPath -WindowStyle Hidden -PassThru
} finally {
    if ($null -eq $previousBrowserArguments) {
        Remove-Item Env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS -ErrorAction SilentlyContinue
    } else {
        $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = $previousBrowserArguments
    }
}

$deadline = [DateTime]::UtcNow.AddSeconds(50)
do {
    Start-Sleep -Milliseconds 500
    $running = Get-Process -Id $newProcess.Id -ErrorAction SilentlyContinue
    $listener = Get-NetTCPConnection -LocalPort 15721 -State Listen -ErrorAction SilentlyContinue
} while (($null -eq $running -or $null -eq $listener) -and [DateTime]::UtcNow -lt $deadline)

if ($null -eq $running -or $null -eq $listener) {
    throw "Updated CC Switch did not become ready within 50 seconds."
}

[pscustomobject]@{
    CurrentProcessId = $newProcess.Id
    InstalledHash    = (Get-FileHash -LiteralPath $installedPath -Algorithm SHA256).Hash
    ExpectedHash     = $sourceHash
    ListenAddress    = $listener.LocalAddress
    ListenPort       = $listener.LocalPort
    OwningProcess    = $listener.OwningProcess
} | ConvertTo-Json -Compress
