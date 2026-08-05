[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [int]$ExpectedProcessId,

    [Parameter(Mandatory = $true)]
    [string]$SourceExecutable,

    [Parameter(Mandatory = $true)]
    [string]$InstalledExecutable,

    [Parameter(Mandatory = $true)]
    [string]$BackupDirectory,

    [int]$RemoteDebuggingPort = 0
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$sourcePath = [System.IO.Path]::GetFullPath($SourceExecutable)
$installedPath = [System.IO.Path]::GetFullPath($InstalledExecutable)
$backupRoot = [System.IO.Path]::GetFullPath($BackupDirectory)
$process = Get-Process -Id $ExpectedProcessId -ErrorAction Stop
$actualPath = [System.IO.Path]::GetFullPath($process.Path)

if (-not [string]::Equals($actualPath, $installedPath, [StringComparison]::OrdinalIgnoreCase)) {
    throw "PID $ExpectedProcessId does not belong to the installed CC Switch executable."
}
if (-not (Test-Path -LiteralPath $sourcePath -PathType Leaf)) {
    throw "Built CC Switch executable is missing: $sourcePath"
}
if (-not (Test-Path -LiteralPath $backupRoot -PathType Container)) {
    throw "Backup directory is missing: $backupRoot"
}

$sourceHash = (Get-FileHash -LiteralPath $sourcePath -Algorithm SHA256).Hash
$installedHash = (Get-FileHash -LiteralPath $installedPath -Algorithm SHA256).Hash
if ([string]::Equals($sourceHash, $installedHash, [StringComparison]::OrdinalIgnoreCase)) {
    throw "Built and installed CC Switch executables are already identical."
}

$backupPath = Join-Path $backupRoot 'cc-switch.exe.before-native-proxy-fix'
Copy-Item -LiteralPath $installedPath -Destination $backupPath -Force
if (-not [string]::Equals(
    (Get-FileHash -LiteralPath $backupPath -Algorithm SHA256).Hash,
    $installedHash,
    [StringComparison]::OrdinalIgnoreCase
)) {
    throw "Installed executable backup hash verification failed."
}

Stop-Process -Id $ExpectedProcessId -ErrorAction Stop
Wait-Process -Id $ExpectedProcessId -Timeout 20 -ErrorAction SilentlyContinue
if (Get-Process -Id $ExpectedProcessId -ErrorAction SilentlyContinue) {
    throw "CC Switch PID $ExpectedProcessId did not stop within 20 seconds."
}

$replacementPath = "$installedPath.replacement-$PID"
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
} catch {
    Copy-Item -LiteralPath $backupPath -Destination $installedPath -Force
    throw
} finally {
    Remove-Item -LiteralPath $replacementPath -Force -ErrorAction SilentlyContinue
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
    PreviousProcessId = $ExpectedProcessId
    CurrentProcessId  = $newProcess.Id
    BackupPath        = $backupPath
    InstalledHash     = (Get-FileHash -LiteralPath $installedPath -Algorithm SHA256).Hash
    ExpectedHash      = $sourceHash
    ListenAddress     = $listener.LocalAddress
    ListenPort        = $listener.LocalPort
    OwningProcess     = $listener.OwningProcess
} | ConvertTo-Json -Compress
