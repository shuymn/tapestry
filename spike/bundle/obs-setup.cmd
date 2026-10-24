@echo off
rem One-time per PC (design 4.1): writes the OBS profiles and the scene
rem collection to obs\generated for the game's native resolution (the
rem collection names this bundle's overlay page by its absolute path, and
rem fits the game and the overlay to the canvas).
rem   obs-setup.cmd --monitor DISPLAY1     (the display the game runs on)
rem   obs-setup.cmd --canvas 2560x1440
rem In OBS: Profile ^> Import each folder under obs\generated\profiles, Scene
rem Collection ^> Import obs\generated\scenes\tapestry-spike.json. Then check
rem the ingest once: run-obs.cmd obs-check ^<profile^> ^<encoder^> --duration-s 40 --ingest-check
setlocal
cd /d "%~dp0"
.\tapestry-spike.exe obs-files --out obs\generated --overlay obs\overlay.html %*
exit /b %errorlevel%
