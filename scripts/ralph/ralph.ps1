# Ralph Wiggum - Long-running Codex agent loop for PowerShell
# Usage: .\ralph.ps1 [-MaxIterations 10] [-ProjectRoot <path>] [-RalphDir <path>] [-Model <model>]

[CmdletBinding()]
param(
    [int]$MaxIterations = 10,
    [string]$ProjectRoot = (Get-Location).Path,
    [string]$RalphDir = $PSScriptRoot,
    [string]$Model = "",
    [string]$RunId = "",
    [switch]$AllowDirtyContinuation,
    [switch]$DryRun
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
[Console]::OutputEncoding = [Text.Encoding]::UTF8

if ($MaxIterations -lt 1 -and -not $DryRun) {
    throw "MaxIterations must be 1 or greater unless -DryRun is specified"
}

# Resolves Codex CLI even before a fresh shell has picked up npm PATH changes.
function Resolve-RalphCodexCommand {
    $codexCommand = Get-Command codex -ErrorAction SilentlyContinue
    if ($null -ne $codexCommand) {
        return $codexCommand.Source
    }

    $candidates = @()
    if (-not [string]::IsNullOrWhiteSpace($env:APPDATA)) {
        $candidates += (Join-Path $env:APPDATA "npm\codex.cmd")
        $candidates += (Join-Path $env:APPDATA "npm\codex.ps1")
    }

    foreach ($candidate in $candidates) {
        if (Test-Path -LiteralPath $candidate) {
            return $candidate
        }
    }

    throw "codex CLI is required but was not found in PATH or the npm global bin directory."
}

# Resolves git even before a fresh shell has picked up PATH changes.
function Resolve-RalphGitCommand {
    $gitCommand = Get-Command git -ErrorAction SilentlyContinue
    if ($null -ne $gitCommand) {
        return $gitCommand.Source
    }

    $programRoots = @($env:ProgramFiles, ${env:ProgramFiles(x86)}) |
        Where-Object { -not [string]::IsNullOrWhiteSpace($_) } |
        Select-Object -Unique

    $candidates = @()
    foreach ($programRoot in $programRoots) {
        $candidates += (Join-Path $programRoot "Git\cmd\git.exe")
        $candidates += (Join-Path $programRoot "Git\bin\git.exe")
    }

    foreach ($candidate in $candidates) {
        if (Test-Path -LiteralPath $candidate) {
            return $candidate
        }
    }

    throw "git is required but was not found in PATH or standard Windows install paths."
}

# Converts a Ralph branch name into a filesystem-safe feature folder name.
function Get-RalphFeatureName {
    param([string]$BranchName)

    $name = $BranchName -replace '^ralph/', ''
    $name = $name -replace '[^\w.-]+', '-'
    if ([string]::IsNullOrWhiteSpace($name)) {
        return "unknown-feature"
    }
    return $name
}

# Creates the progress log with a standard header when it does not already exist.
function Initialize-RalphProgress {
    param([string]$ProgressFile)

    if (-not (Test-Path -LiteralPath $ProgressFile)) {
        "# Ralph Progress Log" | Set-Content -LiteralPath $ProgressFile -Encoding UTF8
        "Started: $(Get-Date -Format o)" | Add-Content -LiteralPath $ProgressFile -Encoding UTF8
        "---" | Add-Content -LiteralPath $ProgressFile -Encoding UTF8
    }
}

# Reads and parses the Ralph PRD JSON file.
function Read-RalphPrd {
    param([string]$PrdFile)

    if (-not (Test-Path -LiteralPath $PrdFile)) {
        throw "Missing prd.json at $PrdFile"
    }

    return Get-Content -LiteralPath $PrdFile -Raw -Encoding UTF8 | ConvertFrom-Json
}

# Returns all executable subtasks, normalizing legacy stories that lack subtasks.
function Get-RalphPrdSubtasks {
    param([object]$Prd)

    $items = @()
    foreach ($story in @($Prd.userStories | Sort-Object priority)) {
        $subtasks = @()
        if ($null -ne $story.PSObject.Properties["subtasks"] -and @($story.subtasks).Count -gt 0) {
            $subtasks = @($story.subtasks | Sort-Object priority, id)
        } else {
            $subtasks = @([pscustomobject]@{
                id = "$([string]$story.id)-ST-001"
                title = [string]$story.title
                description = [string]$story.description
                acceptanceCriteria = @($story.acceptanceCriteria)
                priority = [int]$story.priority
                passes = ($story.passes -eq $true)
                notes = ""
                dependsOn = @()
                parallelSafe = $false
                estimatedFiles = @()
                touches = @("story/$([string]$story.id)")
                stateWrites = @()
                fileBudget = 3
                sourceStoryId = [string]$story.id
            })
        }

        foreach ($subtask in $subtasks) {
            $items += [pscustomobject]@{
                StoryId = [string]$story.id
                StoryTitle = [string]$story.title
                Story = $story
                Subtask = $subtask
            }
        }
    }

    return $items
}

# Returns true when a subtask's dependencies are all completed.
function Test-RalphSubtaskDependenciesSatisfied {
    param(
        [object]$Subtask,
        [string[]]$CompletedSubtaskIds
    )

    $dependsOn = @($Subtask.dependsOn)
    if ($dependsOn.Count -eq 0) {
        return $true
    }

    foreach ($dependency in $dependsOn) {
        if ($CompletedSubtaskIds -notcontains [string]$dependency) {
            return $false
        }
    }

    return $true
}

# Recomputes parent story completion from subtask state and persists when needed.
function Sync-RalphStoryPasses {
    param([string]$PrdFile)

    $prd = Read-RalphPrd -PrdFile $PrdFile
    $changed = $false

    foreach ($story in @($prd.userStories)) {
        $normalizedSubtasks = @()
        if ($null -ne $story.PSObject.Properties["subtasks"] -and @($story.subtasks).Count -gt 0) {
            $normalizedSubtasks = @($story.subtasks)
        } else {
            $normalizedSubtasks = @([pscustomobject]@{
                id = "$([string]$story.id)-ST-001"
                title = [string]$story.title
                description = [string]$story.description
                acceptanceCriteria = @($story.acceptanceCriteria)
                priority = [int]$story.priority
                passes = ($story.passes -eq $true)
                notes = ""
                dependsOn = @()
                parallelSafe = $false
                estimatedFiles = @()
                touches = @("story/$([string]$story.id)")
                stateWrites = @()
                fileBudget = 3
                sourceStoryId = [string]$story.id
            })
            $story | Add-Member -NotePropertyName "subtasks" -NotePropertyValue $normalizedSubtasks -Force
            $changed = $true
        }

        $storyPasses = (@($normalizedSubtasks | Where-Object { $_.passes -ne $true }).Count -eq 0)
        if (($story.passes -eq $true) -ne $storyPasses) {
            $story.passes = $storyPasses
            $changed = $true
        }
    }

    if ($changed) {
        $prd | ConvertTo-Json -Depth 30 | Set-Content -LiteralPath $PrdFile -Encoding UTF8
    }

    return $prd
}

# Returns the highest-priority executable subtask, or null when none remain.
function Get-RalphNextExecutableSubtask {
    param([string]$PrdFile)

    $prd = Sync-RalphStoryPasses -PrdFile $PrdFile
    $items = @(Get-RalphPrdSubtasks -Prd $prd)
    $completedSubtaskIds = @(
        $items |
            Where-Object { $_.Subtask.passes -eq $true } |
            ForEach-Object { [string]$_.Subtask.id }
    )

    $pending = @(
        $items |
            Where-Object { $_.Subtask.passes -ne $true } |
            Sort-Object @{ Expression = { [int]$_.Story.priority } }, @{ Expression = { [int]$_.Subtask.priority } }, @{ Expression = { [string]$_.Subtask.id } }
    )

    foreach ($item in $pending) {
        if (Test-RalphSubtaskDependenciesSatisfied -Subtask $item.Subtask -CompletedSubtaskIds $completedSubtaskIds) {
            return $item
        }
    }

    return $null
}

# Returns a generated Ralph run identifier when none was provided by the caller.
function Resolve-RalphRunId {
    param([string]$RequestedRunId)

    if (-not [string]::IsNullOrWhiteSpace($RequestedRunId)) {
        return $RequestedRunId
    }

    return "$(Get-Date -Format 'yyyyMMdd-HHmmss')-RunProject-$PID"
}

# Appends a session header to the progress log once per run.
function Write-RalphSessionHeader {
    param(
        [string]$ProgressFile,
        [string]$ProjectRoot,
        [string]$RunId,
        [bool]$DirtyContinuation
    )

    Initialize-RalphProgress -ProgressFile $ProgressFile
    $existing = Get-Content -LiteralPath $ProgressFile -Raw -Encoding UTF8
    if ($existing -match [regex]::Escape("Run ID: $RunId")) {
        return
    }

    $branch = (& $GitCommand -C $ProjectRoot branch --show-current 2>$null)
    "" | Add-Content -LiteralPath $ProgressFile -Encoding UTF8
    "## $(Get-Date -Format 'yyyy-MM-dd HH:mm zzz') - Ralph session" | Add-Content -LiteralPath $ProgressFile -Encoding UTF8
    "- Mode: RunProject" | Add-Content -LiteralPath $ProgressFile -Encoding UTF8
    "- Run ID: $RunId" | Add-Content -LiteralPath $ProgressFile -Encoding UTF8
    "- Branch: $branch" | Add-Content -LiteralPath $ProgressFile -Encoding UTF8
    "- Dirty continuation: $DirtyContinuation" | Add-Content -LiteralPath $ProgressFile -Encoding UTF8
    "- Started: $(Get-Date -Format o)" | Add-Content -LiteralPath $ProgressFile -Encoding UTF8
    "---" | Add-Content -LiteralPath $ProgressFile -Encoding UTF8
}

# Archives prior run state when the requested PRD branch changes.
function Invoke-RalphArchiveIfNeeded {
    param(
        [string]$PrdFile,
        [string]$ProgressFile,
        [string]$ArchiveDir,
        [string]$LastBranchFile
    )

    if (-not (Test-Path -LiteralPath $PrdFile)) {
        return
    }

    $prd = Read-RalphPrd -PrdFile $PrdFile
    $currentBranch = [string]($prd.branchName)
    if ([string]::IsNullOrWhiteSpace($currentBranch)) {
        return
    }

    $lastBranch = ""
    if (Test-Path -LiteralPath $LastBranchFile) {
        $lastBranch = (Get-Content -LiteralPath $LastBranchFile -Raw -Encoding UTF8).Trim()
    }

    if (-not [string]::IsNullOrWhiteSpace($lastBranch) -and $lastBranch -ne $currentBranch) {
        $date = Get-Date -Format "yyyy-MM-dd"
        $folderName = Get-RalphFeatureName -BranchName $lastBranch
        $archiveFolder = Join-Path $ArchiveDir "$date-$folderName"

        Write-Host "Archiving previous run: $lastBranch"
        New-Item -ItemType Directory -Force -Path $archiveFolder | Out-Null
        Copy-Item -LiteralPath $PrdFile -Destination (Join-Path $archiveFolder "prd.json") -Force
        if (Test-Path -LiteralPath $ProgressFile) {
            Copy-Item -LiteralPath $ProgressFile -Destination (Join-Path $archiveFolder "progress.txt") -Force
        }

        "# Ralph Progress Log" | Set-Content -LiteralPath $ProgressFile -Encoding UTF8
        "Started: $(Get-Date -Format o)" | Add-Content -LiteralPath $ProgressFile -Encoding UTF8
        "---" | Add-Content -LiteralPath $ProgressFile -Encoding UTF8
    }

    $currentBranch | Set-Content -LiteralPath $LastBranchFile -Encoding UTF8
}

# Builds the stable prompt prefix that should stay identical across runs for better prompt-cache reuse.
function New-RalphStablePromptPrefix {
    param([string]$CodexFile)

    $instructions = Get-Content -LiteralPath $CodexFile -Raw -Encoding UTF8
    return @"
Ralph Stable Instructions:
The following instructions are intentionally placed before runtime-specific values so repeated Ralph runs can share a stable prompt prefix. Keep project paths, iteration numbers, timestamps, log paths, and story-specific data out of this section.

$instructions
"@
}

# Builds the runtime-specific prompt tail for the current iteration.
function New-RalphDynamicPromptTail {
    param(
        [string]$ProjectRoot,
        [string]$ScriptDir,
        [string]$PrdFile,
        [string]$ProgressFile,
        [string]$LogFile,
        [object]$CurrentSubtask,
        [bool]$DirtyContinuation,
        [string]$CurrentBranch
    )

    $subtaskSummary = "No executable subtask selected."
    if ($null -ne $CurrentSubtask) {
        $criteria = @($CurrentSubtask.Subtask.acceptanceCriteria | ForEach-Object { "- $_" }) -join "`n"
        if ([string]::IsNullOrWhiteSpace($criteria)) {
            $criteria = "- No explicit acceptance criteria"
        }

        $dependsOn = @($CurrentSubtask.Subtask.dependsOn)
        $dependsSummary = if ($dependsOn.Count -gt 0) { $dependsOn -join ", " } else { "none" }

        $subtaskSummary = @"
- Current story: $([string]$CurrentSubtask.StoryId) - $([string]$CurrentSubtask.StoryTitle)
- Current subtask: $([string]$CurrentSubtask.Subtask.id) - $([string]$CurrentSubtask.Subtask.title)
- Depends on: $dependsSummary
- Acceptance criteria:
$criteria
"@
    }

    return @"

Ralph Runtime Context:
- Script directory: $ScriptDir
- PRD file: $PrdFile
- Progress file: $ProgressFile
- Log file: $LogFile
- Invocation working directory: $ProjectRoot
- Current branch: $CurrentBranch
- Dirty continuation: $DirtyContinuation
$subtaskSummary
"@
}

# Builds the full prompt with stable instructions first and dynamic runtime context last.
function New-RalphPrompt {
    param(
        [string]$ProjectRoot,
        [string]$ScriptDir,
        [string]$PrdFile,
        [string]$ProgressFile,
        [string]$CodexFile,
        [string]$LogFile,
        [object]$CurrentSubtask,
        [bool]$DirtyContinuation,
        [string]$CurrentBranch
    )

    $stablePrefix = New-RalphStablePromptPrefix -CodexFile $CodexFile
    $dynamicTail = New-RalphDynamicPromptTail -ProjectRoot $ProjectRoot -ScriptDir $ScriptDir -PrdFile $PrdFile -ProgressFile $ProgressFile -LogFile $LogFile -CurrentSubtask $CurrentSubtask -DirtyContinuation $DirtyContinuation -CurrentBranch $CurrentBranch
    return $stablePrefix + $dynamicTail
}

# Runs one non-interactive Codex CLI iteration and captures its output.
function Invoke-CodexIteration {
    param(
        [string]$ProjectRoot,
        [string]$ScriptDir,
        [string]$PrdFile,
        [string]$ProgressFile,
        [string]$CodexFile,
        [string]$LogFile,
        [string]$Model,
        [object]$CurrentSubtask,
        [bool]$DirtyContinuation,
        [string]$CurrentBranch
    )

    $prompt = New-RalphPrompt -ProjectRoot $ProjectRoot -ScriptDir $ScriptDir -PrdFile $PrdFile -ProgressFile $ProgressFile -CodexFile $CodexFile -LogFile $LogFile -CurrentSubtask $CurrentSubtask -DirtyContinuation $DirtyContinuation -CurrentBranch $CurrentBranch
    $args = @(
        "exec",
        "--dangerously-bypass-approvals-and-sandbox",
        "-C",
        $ProjectRoot
    )

    if (-not [string]::IsNullOrWhiteSpace($Model)) {
        $args += @("-m", $Model)
    }

    $args += "-"
    $output = $prompt | & $CodexCommand @args 2>&1
    $status = $LASTEXITCODE
    $logDirectory = Split-Path -Parent $LogFile
    if (-not [string]::IsNullOrWhiteSpace($logDirectory)) {
        New-Item -ItemType Directory -Force -Path $logDirectory | Out-Null
    }
    $output | Set-Content -LiteralPath $LogFile -Encoding UTF8
    if ($status -ne 0) {
        $output | Select-Object -Last 40 | ForEach-Object { Write-Host $_ }
    } else {
        $outputText = $output -join "`n"
        if ($outputText -match "<promise>COMPLETE</promise>") {
            Write-Host "<promise>COMPLETE</promise>"
        } else {
            Write-Host "Codex iteration completed. Full output saved to $LogFile"
        }
    }
    return ,([pscustomobject]@{
        Output = ($output -join "`n")
        ExitCode = $status
    })
}

# Returns true only when every PRD story is marked as passing.
function Test-RalphPrdComplete {
    param([string]$PrdFile)

    $prd = Sync-RalphStoryPasses -PrdFile $PrdFile
    $unfinished = @($prd.userStories | Where-Object { $_.passes -ne $true })
    return $unfinished.Count -eq 0
}

# Returns true only when the latest progress entry reports final deliverable paths.
function Test-RalphDeliverablesReported {
    param([string]$ProgressFile)

    if (-not (Test-Path -LiteralPath $ProgressFile)) {
        return $false
    }

    $content = Get-Content -LiteralPath $ProgressFile -Raw -Encoding UTF8
    if ([string]::IsNullOrWhiteSpace($content)) {
        return $false
    }

    $matches = [regex]::Matches($content, '(?ms)^## .+?(?=^## |\z)')
    if ($matches.Count -eq 0) {
        return $false
    }

    $latest = $matches[$matches.Count - 1].Value
    if (-not ($latest -match '(?m)^- Final deliverables:\s*$')) {
        return $false
    }

    if ($latest -match '(?m)^  none\s*$') {
        return $true
    }

    return $latest -match '(?m)^  [A-Za-z]:\\.+$'
}

$ProjectRoot = (Resolve-Path -LiteralPath $ProjectRoot).Path
$RalphDir = (Resolve-Path -LiteralPath $RalphDir).Path
$PrdFile = Join-Path $RalphDir "prd.json"
$ProgressFile = Join-Path $RalphDir "progress.txt"
$ArchiveDir = Join-Path $RalphDir "archive"
$LastBranchFile = Join-Path $RalphDir ".last-branch"
$CodexFile = Join-Path $RalphDir "CODEX.md"
$RunsDir = Join-Path $RalphDir "runs"
$CodexCommand = Resolve-RalphCodexCommand
$GitCommand = Resolve-RalphGitCommand
$RunId = Resolve-RalphRunId -RequestedRunId $RunId

if (-not (Test-Path -LiteralPath $CodexFile)) {
    throw "Missing CODEX.md at $CodexFile"
}

if ($DryRun) {
    Write-Host "Starting Ralph - Tool: codex - Max iterations: $MaxIterations"
    Write-Host "Project root: $ProjectRoot"
    Write-Host "Ralph dir: $RalphDir"
    Write-Host "Dry run complete. No Ralph state was changed and no Codex iteration was started."
    exit 0
}

$null = Read-RalphPrd -PrdFile $PrdFile
Invoke-RalphArchiveIfNeeded -PrdFile $PrdFile -ProgressFile $ProgressFile -ArchiveDir $ArchiveDir -LastBranchFile $LastBranchFile
Initialize-RalphProgress -ProgressFile $ProgressFile
$CurrentBranch = (& $GitCommand -C $ProjectRoot branch --show-current 2>$null)
Write-RalphSessionHeader -ProgressFile $ProgressFile -ProjectRoot $ProjectRoot -RunId $RunId -DirtyContinuation $AllowDirtyContinuation.IsPresent
New-Item -ItemType Directory -Force -Path $RunsDir | Out-Null

Write-Host "Starting Ralph - Tool: codex - Max iterations: $MaxIterations"
Write-Host "Project root: $ProjectRoot"
Write-Host "Ralph dir: $RalphDir"
if ($AllowDirtyContinuation) {
    Write-Host "Dirty continuation: true (current branch baseline: $CurrentBranch)"
}

for ($i = 1; $i -le $MaxIterations; $i++) {
    $currentSubtask = Get-RalphNextExecutableSubtask -PrdFile $PrdFile
    if ($null -eq $currentSubtask) {
        if (Test-RalphPrdComplete -PrdFile $PrdFile) {
            Write-Host ""
            Write-Host "Ralph completed all tasks."
            Write-Host "No remaining executable subtasks."
            exit 0
        }

        throw "No executable subtasks remain, but prd.json is not complete. Check dependsOn cycles or incomplete subtask state."
    }

    Write-Host ""
    Write-Host "==============================================================="
    Write-Host "  Ralph Iteration $i of $MaxIterations (codex)"
    Write-Host "==============================================================="
    Write-Host "Story: $([string]$currentSubtask.StoryId) - $([string]$currentSubtask.StoryTitle)"
    Write-Host "Subtask: $([string]$currentSubtask.Subtask.id) - $([string]$currentSubtask.Subtask.title)"

    $stamp = Get-Date -Format "yyyyMMdd-HHmmss"
    $logFile = Join-Path $RunsDir "$RunId-$stamp-iteration-$i.log"
    $result = Invoke-CodexIteration -ProjectRoot $ProjectRoot -ScriptDir $RalphDir -PrdFile $PrdFile -ProgressFile $ProgressFile -CodexFile $CodexFile -LogFile $logFile -Model $Model -CurrentSubtask $currentSubtask -DirtyContinuation $AllowDirtyContinuation.IsPresent -CurrentBranch $CurrentBranch

    if ($result.ExitCode -ne 0) {
        Write-Host "Codex iteration failed with exit code $($result.ExitCode)"
        exit $result.ExitCode
    }

    $null = Sync-RalphStoryPasses -PrdFile $PrdFile

    if ($result.Output -match "<promise>COMPLETE</promise>" -and (Test-RalphPrdComplete -PrdFile $PrdFile) -and (Test-RalphDeliverablesReported -ProgressFile $ProgressFile)) {
        Write-Host ""
        Write-Host "Ralph completed all tasks."
        Write-Host "Completed at iteration $i of $MaxIterations"
        exit 0
    }

    if ($result.Output -match "<promise>COMPLETE</promise>") {
        if (-not (Test-RalphPrdComplete -PrdFile $PrdFile)) {
            Write-Host "Codex emitted completion signal, but prd.json still has unfinished stories. Continuing..."
        } elseif (-not (Test-RalphDeliverablesReported -ProgressFile $ProgressFile)) {
            Write-Host "Codex emitted completion signal, but progress.txt is missing final deliverable paths. Continuing..."
        }
    }

    Write-Host "Iteration $i complete. Continuing..."
    Start-Sleep -Seconds 2
}

Write-Host ""
Write-Host "Ralph reached max iterations ($MaxIterations) without completing all tasks."
Write-Host "Check $ProgressFile for status."
exit 1
