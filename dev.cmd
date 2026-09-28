@echo off
rem Builds and starts the dev copy of HQ. It runs next to the installed HQ with its own sessions
rem (profile "dev"), reads ui\ from disk so F5 shows UI edits without a rebuild, and opens
rem DevTools on port 9223 (set HQ_DEBUG_PORT to change it).
setlocal
set "EXE=%~dp0target\debug\hq.exe"
rem A running dev copy locks its exe, so stop that exact file (never the installed HQ) first.
powershell -NoProfile -Command "Get-Process hq -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $env:EXE } | Stop-Process -Force"
call "%~dp0build.cmd" || exit /b 1
start "" "%EXE%"
