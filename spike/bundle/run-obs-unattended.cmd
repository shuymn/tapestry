@echo off
rem Unattended R2/R3 OBS run (design 4.3): as run-obs.cmd, but nobody is at
rem the PC. After the run measure stops the stream through OBS's WebSocket
rem server (enable it in OBS: Tools ^> WebSocket Server Settings) and closes
rem OBS itself; the ingest is recorded to runs\<name>\private\ingest\ for your
rem later review. The result stays provisional (exit 4) until you answer
rem game_visible and audio_present in this run's own runs\<name>\owner-review.toml
rem from its recording and rerun: tapestry-spike.exe report runs\<name>
rem Add --gpu-power-mode <value> (README).
rem   run-obs-unattended.cmd <run-name> <obs-profile> <hardware^|x264> [run options]
setlocal
cd /d "%~dp0"
if "%~3"=="" (
  echo usage: run-obs-unattended.cmd ^<run-name^> ^<obs-profile^> ^<hardware^|x264^> [run options]
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
.\tapestry-spike.exe measure --unattended --presentmon presentmon\PresentMon-2.6.0-x64.exe --ingest mediamtx\mediamtx.exe --ingest-config obs\mediamtx.yml --obs "%ProgramFiles%\obs-studio\bin\64bit\obs64.exe" --obs-profile %PROFILE% --obs-collection tapestry-spike --obs-encoder %ENCODER% --bga assets\bga.mp4 --out runs\%NAME% %EXTRA%
set RESULT=%errorlevel%
if not "%RESULT%"=="0" exit /b %RESULT%
.\tapestry-spike.exe report runs\%NAME%
set RESULT=%errorlevel%
call :ladder
echo Review runs\%NAME%\private\ingest\ (game visible, audio present, hitches, tearing, BGA, crackles),
echo answer in runs\%NAME%\owner-review.toml (this run only), then rerun: tapestry-spike.exe report runs\%NAME%
echo Hand-off: only runs\%NAME%\export\*.zip leaves this PC.
exit /b %RESULT%

rem Prints the commands for the rungs report wrote to ladder.txt.
:ladder
if not exist runs\%NAME%\ladder.txt exit /b 0
for /f "usebackq tokens=1,* delims==" %%a in ("runs\%NAME%\ladder.txt") do (
  if "%%a"=="profile" echo Next, profile rung: run-obs-unattended.cmd %NAME%-streaming %PROFILE% %ENCODER%%EXTRA% %%b
  if "%%a"=="audio" echo Next, audio period rung: run-obs-unattended.cmd %NAME%-period %PROFILE% %ENCODER%%EXTRA% %%b
  if "%%a"=="note" echo Ladder: %%b
)
exit /b 0
