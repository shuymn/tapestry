@echo off
rem S0 starvation check (design 4.2, D5): 2 minutes with writes withheld
rem 50 ms every 20 s from 10 s, and 2 minutes with a 2 ms gap injected every
rem 10 s; diagnostic runs. Each run checks its own audio evidence: every
rem withholding must show a discontinuity episode and empty-buffer wakes,
rem every injected gap exactly one episode. S0 passes only if both do.
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
set STARVE=%errorlevel%
.\tapestry-spike.exe run --duration-s 120 --inject-gap-every-s 10 --inject-gap-ms 2 --bga assets\bga.mp4 --out runs\%NAME%-inject %EXTRA%
set INJECT=%errorlevel%
if "%STARVE%%INJECT%"=="00" (
  echo S0: pass
  exit /b 0
)
echo S0: FAIL - starvation run exit %STARVE%, injection run exit %INJECT%
echo See the "run: audio check failed" lines above and "check" in runs\%NAME%-starve\play.json and runs\%NAME%-inject\play.json.
exit /b 1
