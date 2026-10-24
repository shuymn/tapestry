@echo off
rem S0 starvation check (design 4.2, D5): 2 minutes with writes withheld
rem 50 ms every 20 s from 10 s, and the injected-gap check; diagnostic runs.
rem   run-starve.cmd <run-name> [run options]
setlocal
cd /d "%~dp0"
if "%~1"=="" (
  echo usage: run-starve.cmd ^<run-name^> [run options]
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
.\tapestry-spike.exe run --duration-s 120 --starve-ms 50 --bga assets\bga.mp4 --out runs\%NAME%-starve %EXTRA%
.\tapestry-spike.exe run --duration-s 120 --inject-gap-every-s 10 --inject-gap-ms 2 --bga assets\bga.mp4 --out runs\%NAME%-inject %EXTRA%
exit /b %errorlevel%
