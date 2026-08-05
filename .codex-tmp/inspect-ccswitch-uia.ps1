[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [int]$ProcessId,

    [int]$MaxDepth = 24
)

Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes

$root = [System.Windows.Automation.AutomationElement]::RootElement
$condition = New-Object System.Windows.Automation.PropertyCondition(
    [System.Windows.Automation.AutomationElement]::ProcessIdProperty,
    $ProcessId
)
$window = $root.FindFirst(
    [System.Windows.Automation.TreeScope]::Children,
    $condition
)

if ($null -eq $window) {
    throw "CC Switch window not found for PID $ProcessId"
}

$walker = [System.Windows.Automation.TreeWalker]::RawViewWalker
$queue = [System.Collections.Generic.Queue[object]]::new()
$queue.Enqueue([pscustomobject]@{ Element = $window; Depth = 0 })

while ($queue.Count -gt 0) {
    $item = $queue.Dequeue()
    $element = $item.Element
    $depth = [int]$item.Depth
    $current = $element.Current

    if ($current.Name -or $current.AutomationId -or $depth -eq 0) {
        [pscustomobject]@{
            Depth        = $depth
            ControlType  = $current.ControlType.ProgrammaticName
            Name         = $current.Name
            AutomationId = $current.AutomationId
            IsEnabled    = $current.IsEnabled
            IsOffscreen  = $current.IsOffscreen
        }
    }

    if ($depth -ge $MaxDepth) {
        continue
    }

    $child = $walker.GetFirstChild($element)
    while ($null -ne $child) {
        $queue.Enqueue([pscustomobject]@{ Element = $child; Depth = $depth + 1 })
        $child = $walker.GetNextSibling($child)
    }
}
