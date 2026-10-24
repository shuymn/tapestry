@echo off
rem One-time per PC (design 4.1): measurement conditions. Fill owner-notes.toml
rem first; conditions.json stays on this PC, only the printed fields leave it.
setlocal
cd /d "%~dp0"
.\tapestry-spike.exe conditions --out . --bga assets\bga.mp4 %*
exit /b %errorlevel%
