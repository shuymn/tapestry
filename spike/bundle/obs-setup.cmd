@echo off
rem One-time per PC (design 4.1): writes the OBS profiles and the scene
rem collection to obs\generated (the collection names this bundle's overlay
rem page by its absolute path). In OBS: Profile ^> Import each folder under
rem obs\generated\profiles, Scene Collection ^> Import
rem obs\generated\scenes\tapestry-spike.json, then Settings ^> Video ^> Base
rem (Canvas) Resolution = the game's resolution. Confirm the game capture
rem shows the spike window during a run.
setlocal
cd /d "%~dp0"
.\tapestry-spike.exe obs-files --out obs\generated --overlay obs\overlay.html
exit /b %errorlevel%
