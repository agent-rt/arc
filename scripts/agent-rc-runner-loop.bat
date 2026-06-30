@echo off
rem Build-aware supervisor for the runner. Credentials/endpoint come from
rem %APPDATA%\arc\runner.toml (written by `arc-runner pair`), so none
rem are baked into this script.
set PATH=%USERPROFILE%\.cargo\bin;%PATH%
cd /d "%~dp0.."
:loop
cargo build -p arc-runner > update.log 2>&1
"target\debug\arc-runner.exe"
ping -n 4 127.0.0.1 >nul
goto loop
