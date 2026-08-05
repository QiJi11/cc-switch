[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [int]$ExpectedProcessId,

    [Parameter(Mandatory = $true)]
    [string]$ExecutablePath,

    [int]$RemoteDebuggingPort = 0
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$expectedExecutable = [System.IO.Path]::GetFullPath($ExecutablePath)
$process = Get-Process -Id $ExpectedProcessId -ErrorAction Stop
$actualExecutable = [System.IO.Path]::GetFullPath($process.Path)
if (-not [string]::Equals($actualExecutable, $expectedExecutable, [StringComparison]::OrdinalIgnoreCase)) {
    throw "PID $ExpectedProcessId does not belong to the expected CC Switch executable."
}

Stop-Process -Id $ExpectedProcessId -ErrorAction Stop
Wait-Process -Id $ExpectedProcessId -Timeout 20 -ErrorAction SilentlyContinue
if (Get-Process -Id $ExpectedProcessId -ErrorAction SilentlyContinue) {
    throw "CC Switch PID $ExpectedProcessId did not stop within 20 seconds."
}

$previousBrowserArguments = $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS
try {
    if ($RemoteDebuggingPort -gt 0) {
        $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = "--remote-debugging-port=$RemoteDebuggingPort"
    } else {
        Remove-Item Env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS -ErrorAction SilentlyContinue
    }
    $newProcess = Start-Process -FilePath $expectedExecutable -WindowStyle Hidden -PassThru
} finally {
    if ($null -eq $previousBrowserArguments) {
        Remove-Item Env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS -ErrorAction SilentlyContinue
    } else {
        $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = $previousBrowserArguments
    }
}
$deadline = [DateTime]::UtcNow.AddSeconds(40)
do {
    Start-Sleep -Milliseconds 500
    $running = Get-Process -Id $newProcess.Id -ErrorAction SilentlyContinue
    $listener = Get-NetTCPConnection -LocalPort 15721 -State Listen -ErrorAction SilentlyContinue
} while (($null -eq $running -or $null -eq $listener) -and [DateTime]::UtcNow -lt $deadline)

if ($null -eq $running) {
    throw "Restarted CC Switch process exited before becoming ready."
}
if ($null -eq $listener) {
    throw "CC Switch restarted, but port 15721 did not become ready within 40 seconds."
}

[pscustomobject]@{
    PreviousProcessId = $ExpectedProcessId
    CurrentProcessId  = $newProcess.Id
    ExecutablePath    = $running.Path
    ListenAddress     = $listener.LocalAddress
    ListenPort        = $listener.LocalPort
    OwningProcess     = $listener.OwningProcess
} | ConvertTo-Json -Compress
