@echo off
rem Manually run the runner in the foreground (debugging). Credentials/endpoint
rem come from %APPDATA%\arc\runner.toml (written by `arc-runner pair`).
set RUST_LOG=info
cd /d "%~dp0.."
"target\debug\arc-runner.exe"
