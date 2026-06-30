@echo off
rem Supervisor for the relay (relay mode only). Restarts it on crash.
set ARC_RELAY_ADDR=0.0.0.0:8787
cd /d "%~dp0.."
:loop
"target\debug\arc-relay.exe"
ping -n 4 127.0.0.1 >nul
goto loop
