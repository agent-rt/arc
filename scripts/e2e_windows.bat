@echo off
setlocal
cd /d "%~dp0.."

rem Local loopback smoke test — throwaway session/pairing, not secrets.
set RUST_LOG=info
set URL=ws://127.0.0.1:8787/v1/relay
set SESSION=00112233445566778899aabbccddeeff
set PAIRING=TEST-1234
set ARC_RELAY_ADDR=127.0.0.1:8787

echo [e2e] starting relay...
start "relay" /b cmd /c "target\debug\arc-relay.exe > relay.log 2>&1"
ping -n 3 127.0.0.1 >nul

echo [e2e] starting runner...
start "runner" /b cmd /c "target\debug\arc-runner.exe %URL% %SESSION% %PAIRING% > runner.log 2>&1"
ping -n 4 127.0.0.1 >nul

echo [e2e] running probe...
target\debug\examples\probe.exe %URL% %SESSION% %PAIRING% > probe.log 2>&1
set RC=%ERRORLEVEL%

echo [e2e] probe exit=%RC%
taskkill /f /im arc-relay.exe >nul 2>&1
taskkill /f /im arc-runner.exe >nul 2>&1

echo probe exit=%RC% >> probe.log
echo [e2e] ===== probe.log =====
type probe.log
echo [e2e] ===== relay.log =====
type relay.log
echo [e2e] ===== runner.log =====
type runner.log
exit /b %RC%
