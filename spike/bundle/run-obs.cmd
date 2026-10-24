@echo off
rem R2/R3 OBS runs (design 4.2): the local RTMP ingest (MediaMTX on
rem 127.0.0.1, started and stopped by measure; refused if the port is
rem already taken), PresentMon, OBS streaming with the imported profile and
rem scene collection, a 20 s countdown and 4 loops. When the run ends, stop
rem streaming and exit OBS at once (V7 allows 60 s after play); the newest
rem OBS log is then collected. After a gate failure the next ladder rung
rem (streaming profile, next audio period) is printed.
rem   run-obs.cmd <run-name> <obs-profile> <hardware|x264> [run options]
rem   e.g. run-obs.cmd r2 tapestry-spike-nvenc hardware
rem        run-obs.cmd r3 tapestry-spike-x264 x264
rem   ingest check (once per PC, diagnostic, 40 s):
rem        run-obs.cmd obs-check tapestry-spike-nvenc hardware --duration-s 40 --ingest-check
setlocal
cd /d "%~dp0"
if "%~3"=="" (
  echo usage: run-obs.cmd ^<run-name^> ^<obs-profile^> ^<hardware^|x264^> [run options]
  exit /b 2
)
set NAME=%~1
set PROFILE=%~2
set ENCODER=%~3
shift
shift
shift
set EXTRA=
:collect
if "%~1"=="" goto run
set EXTRA=%EXTRA% %1
shift
goto collect
:run
.\tapestry-spike.exe measure --presentmon presentmon\PresentMon-2.6.0-x64.exe --ingest mediamtx\mediamtx.exe --ingest-config obs\mediamtx.yml --obs "%ProgramFiles%\obs-studio\bin\64bit\obs64.exe" --obs-profile %PROFILE% --obs-collection tapestry-spike --obs-encoder %ENCODER% --bga assets\bga.mp4 --out runs\%NAME% %EXTRA%
set RESULT=%errorlevel%
if not "%RESULT%"=="0" exit /b %RESULT%
echo Fill game_visible and audio_present in owner-notes.toml (what the ingest showed),
pause
.\tapestry-spike.exe report runs\%NAME%
set RESULT=%errorlevel%
call :ladder
echo Hand-off: only runs\%NAME%\export\*.zip leaves this PC.
exit /b %RESULT%

rem Prints the commands for the rungs report wrote to ladder.txt.
:ladder
if not exist runs\%NAME%\ladder.txt exit /b 0
for /f "usebackq tokens=1,* delims==" %%a in ("runs\%NAME%\ladder.txt") do (
  if "%%a"=="profile" echo Next, profile rung: run-obs.cmd %NAME%-streaming %PROFILE% %ENCODER%%EXTRA% %%b
  if "%%a"=="audio" echo Next, audio period rung: run-obs.cmd %NAME%-period %PROFILE% %ENCODER%%EXTRA% %%b
  if "%%a"=="note" echo Ladder: %%b
)
exit /b 0
