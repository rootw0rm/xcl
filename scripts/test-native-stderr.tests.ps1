# Run with powershell.exe or pwsh.exe. The fixture writes stderr through a native process.
$ErrorActionPreference = 'Stop'
$runner = Join-Path $PSScriptRoot 'test.ps1'
$fixture = Join-Path ([IO.Path]::GetTempPath()) ('xcl-native-stderr-' + [guid]::NewGuid())
$null = New-Item -ItemType Directory -Path $fixture
$cargo = Join-Path $fixture 'cargo.cmd'
$log = Join-Path $fixture 'runner.log'
$oldPath = $env:PATH
$hadFspx = Test-Path Env:XCL_SAMPLE_FSPX
$oldFspx = $env:XCL_SAMPLE_FSPX
$hadFail = Test-Path Env:XCL_TEST_CARGO_FAIL
$oldFail = $env:XCL_TEST_CARGO_FAIL
Push-Location
try {
    @'
@echo off
if "%1"=="build" goto build
if "%1"=="test" goto test
exit /b 127

:build
if "%XCL_TEST_CARGO_FAIL%"=="1" goto build_fail
>&2 echo synthetic build progress
exit /b 0

:build_fail
>&2 echo synthetic build failure
exit /b 42

:test
>&2 echo SKIPPED native_stderr: synthetic reason
exit /b 0
'@ | Set-Content -LiteralPath $cargo -Encoding Ascii
    $env:PATH = $fixture
    if ((Get-Command cargo).Path -ne $cargo) { throw 'The native Cargo fixture was not selected.' }

    Remove-Item Env:XCL_SAMPLE_FSPX -ErrorAction SilentlyContinue
    Remove-Item Env:XCL_TEST_CARGO_FAIL -ErrorAction SilentlyContinue
    Set-Location -LiteralPath $fixture
    $expectedCwd = (Get-Location).Path

    & $runner *> $log
    if ($LASTEXITCODE -ne 0) { throw "Runner exited $LASTEXITCODE on a successful build." }
    if ((Get-Location).Path -ne $expectedCwd) {
        throw "Expected caller location '$expectedCwd' after successful run, got '$((Get-Location).Path)'."
    }
    $output = Get-Content -LiteralPath $log -Raw
    if (!$output.Contains('synthetic build progress') -or
        !$output.Contains('GPU tests skipped on this adapter: 1 (synthetic reason)')) {
        throw 'Expected native build output and the GPU skip summary in the redirected log.'
    }
    $unwrapped = $output -replace '\r?\n', ''
    if ($unwrapped -notmatch 'not set, their tests are skipped:.*?\bXCL_SAMPLE_FSPX') {
        throw "Expected XCL_SAMPLE_FSPX in the skipped-samples message when unset. Output:`n$output"
    }

    $env:XCL_TEST_CARGO_FAIL = '1'
    & $runner *> $log
    if ($LASTEXITCODE -ne 42) { throw "Runner exited $LASTEXITCODE on a failed build; expected 42." }
    if ((Get-Location).Path -ne $expectedCwd) {
        throw "Expected caller location '$expectedCwd' after failed build, got '$((Get-Location).Path)'."
    }
} finally {
    Pop-Location
    $env:PATH = $oldPath
    if ($hadFspx) { $env:XCL_SAMPLE_FSPX = $oldFspx } else { Remove-Item Env:XCL_SAMPLE_FSPX -ErrorAction SilentlyContinue }
    if ($hadFail) { $env:XCL_TEST_CARGO_FAIL = $oldFail } else { Remove-Item Env:XCL_TEST_CARGO_FAIL -ErrorAction SilentlyContinue }
    foreach ($path in $cargo, $log, $fixture) {
        if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path }
    }
}
