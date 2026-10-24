@echo off
rem R2/R3 OBS runs (design 4.2): the local RTMP ingest (MediaMTX on
rem 127.0.0.1), PresentMon, OBS streaming with the imported profile and scene
rem collection, a 20 s countdown and 4 loops. When the run ends, stop
rem streaming and exit OBS at once (V7 allows 60 s after play); the newest
rem OBS log is then collected.
rem   run-obs.cmd <run-name> <obs-profile> <hardware|x264> [run options]
rem   e.g. run-obs.cmd r2 tapestry-spike-nvenc hardware
rem        run-obs.cmd r3 tapestry-spike-x264 x264
rem   streaming profile: add  --profile streaming --fps-cap 120  (240 Hz PC)
rem                       or  --profile streaming --fps-cap 60   (60 Hz PC)
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
start "spike ingest" /min .\mediamtx\mediamtx.exe obs\mediamtx.yml
.\tapestry-spike.exe measure --presentmon presentmon\PresentMon-2.6.0-x64.exe --obs "%ProgramFiles%\obs-studio\bin\64bit\obs64.exe" --obs-profile %PROFILE% --obs-collection tapestry-spike --obs-encoder %ENCODER% --bga assets\bga.mp4 --out runs\%NAME% %EXTRA%
set RESULT=%errorlevel%
taskkill /im mediamtx.exe >nul 2>&1
if not "%RESULT%"=="0" exit /b %RESULT%
.\tapestry-spike.exe report runs\%NAME%
echo Hand-off: only runs\%NAME%\export\*.zip leaves this PC.
exit /b %errorlevel%
