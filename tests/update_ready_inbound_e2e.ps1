[CmdletBinding()]
param(
    [string]$BinaryPath = '',
    [ValidateRange(5, 30)]
    [int]$TimeoutSeconds = 10
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
if ([string]::IsNullOrWhiteSpace($BinaryPath)) {
    $BinaryPath = Join-Path $repoRoot 'target\debug\gengchou.exe'
}
$BinaryPath = [IO.Path]::GetFullPath($BinaryPath)
if (-not (Test-Path -LiteralPath $BinaryPath -PathType Leaf)) {
    throw "Debug binary not found: $BinaryPath"
}

$e2eRoot = Join-Path $repoRoot 'target\update-ready-inbound-e2e'
$testRoot = Join-Path $e2eRoot ([Guid]::NewGuid().ToString('N'))
$readyDir = Join-Path $testRoot 'ready'
$marker = Join-Path $readyDir 'update-ready-e2e.marker'
New-Item -ItemType Directory -Path $readyDir -Force | Out-Null

# What the last probe actually inherited. A failing probe deletes its own
# evidence with $testRoot, so capture the inputs while they still exist.
$script:LastProbeEnvironment = '(no probe has run yet)'

$InterestingEnvNames = @(
    'GENGCHOU_UPDATE_READY_FILE',
    'GENGCHOU_UPDATE_TEST_READY_DIR',
    'APPDATA',
    'LOCALAPPDATA'
)

function Get-ProbeFailureReport {
    param(
        [hashtable]$Environment,
        [int]$ExitCode
    )

    $log = '(probe environment has no LOCALAPPDATA)'
    if ($Environment.ContainsKey('LOCALAPPDATA')) {
        $logPath = Join-Path ([string]$Environment['LOCALAPPDATA']) 'Gengchou\diagnose.log'
        if (Test-Path -LiteralPath $logPath -PathType Leaf) {
            $log = Get-Content -LiteralPath $logPath -Raw
        } else {
            $log = "(no diagnose.log at $logPath)"
        }
    }

    @"
exit code: $ExitCode
environment the probe inherited:
$script:LastProbeEnvironment
probe diagnose.log:
$log
"@
}

function Invoke-ReadyProbe {
    param(
        [hashtable]$Environment,
        [string]$ProbeBinary = $BinaryPath
    )

    $previous = @{}
    foreach ($name in $Environment.Keys) {
        $previous[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
        [Environment]::SetEnvironmentVariable($name, [string]$Environment[$name], 'Process')
    }
    $script:LastProbeEnvironment = ($InterestingEnvNames | ForEach-Object {
            $value = [Environment]::GetEnvironmentVariable($_, 'Process')
            if ($null -eq $value) { "  $_ = (unset)" } else { "  $_ = $value" }
        }) -join [Environment]::NewLine
    try {
        $process = Start-Process -FilePath $ProbeBinary `
            -ArgumentList '--confirm-update-ready-test' `
            -WorkingDirectory $testRoot `
            -WindowStyle Hidden `
            -PassThru
        if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
            $process.Kill()
            throw 'Update readiness probe timed out.'
        }
        return $process.ExitCode
    }
    finally {
        foreach ($name in $previous.Keys) {
            $value = $previous[$name]
            if ($null -eq $value) {
                # pwsh 7 leaves a present-but-empty variable behind when it is
                # assigned $null, which the next probe would then inherit as a
                # real value. Removing it from the Env: drive works on both
                # Windows PowerShell 5.1 and PowerShell 7.
                Remove-Item -LiteralPath "Env:$name" -ErrorAction SilentlyContinue
            } else {
                [Environment]::SetEnvironmentVariable($name, $value, 'Process')
            }
        }
    }
}

try {
    $environment = @{
        'APPDATA' = (Join-Path $testRoot 'Roaming')
        'LOCALAPPDATA' = (Join-Path $testRoot 'Local')
        'GENGCHOU_UPDATE_TEST_READY_DIR' = $readyDir
        'GENGCHOU_UPDATE_READY_FILE' = $marker
    }
    $exitCode = Invoke-ReadyProbe -Environment $environment
    if ($exitCode -ne 0) {
        throw "Inbound readiness probe failed.`n$(Get-ProbeFailureReport -Environment $environment -ExitCode $exitCode)"
    }
    $content = [IO.File]::ReadAllText($marker, [Text.Encoding]::UTF8)
    if ($content -ne "Gengchou update ready`n") {
        throw 'Update readiness marker content was not preserved exactly.'
    }

    # A confirmed backup is deferred cleanup, not an active transaction. A
    # scanner/indexer lock must not make the otherwise healthy app fail its
    # readiness milestone.
    $normalProbe = Join-Path $testRoot 'normal-start-probe.exe'
    $normalBackup = "$normalProbe.old"
    Copy-Item -LiteralPath $BinaryPath -Destination $normalProbe
    [IO.File]::WriteAllText($normalBackup, 'previous version', [Text.Encoding]::UTF8)
    $lock = [IO.File]::Open(
        $normalBackup,
        [IO.FileMode]::Open,
        [IO.FileAccess]::ReadWrite,
        [IO.FileShare]::None
    )
    try {
        $normalEnvironment = @{
            'APPDATA' = (Join-Path $testRoot 'NormalRoaming')
            'LOCALAPPDATA' = (Join-Path $testRoot 'NormalLocal')
            'GENGCHOU_UPDATE_TEST_READY_DIR' = $readyDir
        }
        $normalExitCode = Invoke-ReadyProbe `
            -Environment $normalEnvironment `
            -ProbeBinary $normalProbe
        if ($normalExitCode -ne 0) {
            throw "Normal readiness probe with locked backup failed.`n$(Get-ProbeFailureReport -Environment $normalEnvironment -ExitCode $normalExitCode)"
        }
        if (-not (Test-Path -LiteralPath $normalBackup -PathType Leaf)) {
            throw 'The locked confirmed backup was unexpectedly removed.'
        }
    }
    finally {
        $lock.Dispose()
    }

    $cleanupExitCode = Invoke-ReadyProbe `
        -Environment $normalEnvironment `
        -ProbeBinary $normalProbe
    if ($cleanupExitCode -ne 0) {
        throw "Normal readiness cleanup probe failed.`n$(Get-ProbeFailureReport -Environment $normalEnvironment -ExitCode $cleanupExitCode)"
    }
    if (Test-Path -LiteralPath $normalBackup) {
        throw 'The confirmed backup was not removed after its lock was released.'
    }

    Write-Output 'Updater inbound E2E passed: active readiness and locked confirmed-backup cleanup are correct.'
}
finally {
    $resolvedRoot = [IO.Path]::GetFullPath($testRoot)
    $allowedPrefix = [IO.Path]::GetFullPath($e2eRoot).TrimEnd('\') + '\'
    if (-not $resolvedRoot.StartsWith($allowedPrefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Refusing cleanup outside the updater inbound E2E root: $resolvedRoot"
    }
    if (Test-Path -LiteralPath $resolvedRoot) {
        Remove-Item -LiteralPath $resolvedRoot -Recurse -Force
    }
}
