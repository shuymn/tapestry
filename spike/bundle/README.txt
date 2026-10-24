tapestry stage 0 spike bundle (owner procedure, design 4)

Unzip to a short path (for example C:\spike). Nothing here needs
administrator rights; PresentMon runs as your user.

Once per PC
  1. Edit owner-notes.toml (pc_form, vrr, display_connection, ...).
  2. conditions.cmd                  (writes conditions.json, prints what leaves the PC)
  3. obs-setup.cmd --monitor DISPLAY1   (the display the game runs on), then
     import the profiles and the scene collection in OBS (see
     obs-setup.cmd). Keep OBS normal priority, not elevated.
  4. Ingest check (diagnostic, about a minute; exit OBS when asked):
     run-obs.cmd obs-check tapestry-spike-nvenc hardware --duration-s 40 --ingest-check
     It must print "ingest check: pass" (game video and audio reach the
     local ingest).

Before each session: close other apps, laptop on AC, the display at its
exact 60/240 Hz mode with VRR off, Do Not Disturb on, and do not touch the PC
during a run. No other MediaMTX may be running (the OBS runs refuse to start
over it and never stop it).

GPU driver power setting: add --gpu-power-mode <value> to every run to
record the per-program power management setting you chose for
tapestry-spike.exe in the GPU control panel (default, normal, optimal-power,
adaptive, prefer-max-performance, prefer-consistent-performance). It cannot
be read by the spike and is recorded as declared.

Runs (results in runs\<name>; hand off only runs\<name>\export\*.zip)
  run-starve.cmd s0                         S0: prints "S0: pass" or the failure
  run-baseline.cmd r1                       R1: standard, 600 s
  run-obs.cmd r2 tapestry-spike-nvenc hardware   R2: OBS, hardware encoder
  run-obs.cmd r3 tapestry-spike-x264 x264        R3: OBS, x264
  (use tapestry-spike-amf or tapestry-spike-qsv for AMD or Intel encoders)
  After an OBS run, stop streaming and exit OBS at once; fill game_visible and
  audio_present in owner-notes.toml when asked; the report follows. After a
  gate failure the scripts print the next run (streaming profile, or the next
  audio period).

Files: bundle-manifest.json lists every file with its SHA-256; licenses\
holds the FFmpeg (LGPL), PresentMon and MediaMTX (MIT) licenses.
