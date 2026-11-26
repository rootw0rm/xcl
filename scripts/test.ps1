# Build everything (the e2e test needs the test title + DLL) and run all tests.
# Arguments go to the test binaries (after `--`), e.g. `.\scripts\test.ps1 --skip <name>`.
# `-Brief`: the whole output goes to a file (`$env:XCL_TEST_LOG`, else `<target dir>\test-output.txt`);
# the console gets build errors, the failures section, the skip summary and the summed counts.
$ErrorActionPreference = 'Stop'
Push-Location (Join-Path $PSScriptRoot '..')
try {
$brief = $args -contains '-Brief'
$rest = @($args | Where-Object { $_ -ne '-Brief' })
$log = $null
if ($brief) {
    $logPath = $env:XCL_TEST_LOG
    if (-not $logPath) {
        $targetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { 'target' }
        $null = New-Item -ItemType Directory -Force -Path $targetDir
        $logPath = Join-Path (Resolve-Path $targetDir) 'test-output.txt'
    }
    $log = New-Object IO.StreamWriter($logPath, $false, (New-Object Text.UTF8Encoding($false)))
}
# tests on game data pass without running anything when their variable is not set
$missing = 'XCL_SAMPLE_RPF', 'XCL_SAMPLE_DLLS', 'XCL_SAMPLE_TTPF', 'XCL_SAMPLE_BCPACK', 'XCL_SAMPLE_BK2', 'XCL_SAMPLE_BVH4', 'XCL_SAMPLE_AWC', 'XCL_SAMPLE_XG', 'XCL_SAMPLE_DXC', 'XCL_SAMPLE_FSPX' | Where-Object { -not (Test-Path "env:$_") }
if ($missing) { Write-Host "not set, their tests are skipped: $($missing -join ', ')" }
# Windows PowerShell 5.1 treats redirected native stderr as an error even when Cargo succeeds.
$null = Get-Command cargo -ErrorAction Stop
$ErrorActionPreference = 'Continue'
if ($brief) {
    # from the first error on (at most 200 lines), only when the build fails
    $errors = New-Object Collections.Generic.List[string]
    cargo build --workspace 2>&1 | ForEach-Object {
        $line = "$_"
        $log.WriteLine($line)
        if (($errors.Count -gt 0 -or $line -match '^error') -and $errors.Count -lt 200) { $errors.Add($line) }
    }
    if ($LASTEXITCODE -ne 0) {
        $code = $LASTEXITCODE
        $errors | ForEach-Object { Write-Host $_ }
        $log.Close()
        Write-Host "build failed; full output: $logPath"
        exit $code
    }
} else {
    cargo build --workspace
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}
# GPU tests that cannot run on this adapter pass too: each prints "SKIPPED <test>: <reason>"
# to stderr (XCL_REQUIRE_GPU=wave64 / =mesh makes them fail); counted at the end
$skipped = @()
$totals = @{ passed = 0; failed = 0; ignored = 0; binaries = 0 }
$inFailures = $false
cargo test --workspace --no-fail-fast -- --test-threads=4 @rest 2>&1 | ForEach-Object {
    $line = "$_"
    if ($line -match '^SKIPPED \S+: (.+)$') { $skipped += $Matches[1] }
    if (-not $brief) { Write-Host $line; return }
    $log.WriteLine($line)
    if ($line -match '^test result: \w+\. (\d+) passed; (\d+) failed; (\d+) ignored') {
        $totals.passed += [int]$Matches[1]; $totals.failed += [int]$Matches[2]; $totals.ignored += [int]$Matches[3]
        $totals.binaries++
        if ($inFailures) { Write-Host $line }
        $inFailures = $false
        return
    }
    if ($line -match '^failures:$') { $inFailures = $true }
    if ($inFailures -or $line -match '^error') { Write-Host $line }
}
$code = $LASTEXITCODE
if ($skipped) {
    $counts = $skipped | Group-Object | Sort-Object Count -Descending | ForEach-Object { "$($_.Count) ($($_.Name))" }
    Write-Host "GPU tests skipped on this adapter: $($counts -join ', ')"
}
if ($brief) {
    $log.Close()
    Write-Host "tests: $($totals.passed) passed, $($totals.failed) failed, $($totals.ignored) ignored ($($totals.binaries) test binaries); exit $code; full output: $logPath"
}
exit $code
} finally {
    Pop-Location
}
