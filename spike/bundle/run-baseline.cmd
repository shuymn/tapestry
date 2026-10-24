@echo off
rem R1 baseline (design 4.2): standard profile, no cap, BGA on, 4 loops (600 s),
rem under PresentMon, then the report and the export.
rem   run-baseline.cmd <run-name> [more run options, e.g. --monitor DISPLAY1]
setlocal
cd /d "%~dp0"
if "%~1"=="" (
  echo usage: run-baseline.cmd ^<run-name^> [run options]
  exit /b 2
)
set NAME=%~1
shift
set EXTRA=
:collect
if "%~1"=="" goto run
set EXTRA=%EXTRA% %1
shift
goto collect
:run
.\tapestry-spike.exe measure --presentmon presentmon\PresentMon-2.6.0-x64.exe --profile standard --bga assets\bga.mp4 --out runs\%NAME% %EXTRA%
if errorlevel 1 exit /b %errorlevel%
.\tapestry-spike.exe report runs\%NAME%
echo Hand-off: only runs\%NAME%\export\*.zip leaves this PC.
exit /b %errorlevel%
