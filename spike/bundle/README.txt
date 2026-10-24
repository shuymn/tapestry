tapestry stage 0 spike bundle (owner procedure, design 4)

Unzip to a short path (for example C:\spike). Nothing here needs
administrator rights; PresentMon runs as your user.

Once per PC
  1. Edit owner-notes.toml (pc_form, vrr, display_connection, ...).
  2. conditions.cmd                  (writes conditions.json, prints what leaves the PC)
  3. obs-setup.cmd, then import the profiles and the scene collection in OBS
     (see obs-setup.cmd). Keep OBS normal priority, not elevated.

Before each session: close other apps, laptop on AC, the display at its
exact 60/240 Hz mode with VRR off, Do Not Disturb on, and do not touch the PC
during a run.

Runs (results in runs\<name>; hand off only runs\<name>\export\*.zip)
  run-starve.cmd s0                         S0: starvation and injected-gap checks
  run-baseline.cmd r1                       R1: standard, 600 s
  run-obs.cmd r2 tapestry-spike-nvenc hardware   R2: OBS, hardware encoder
  run-obs.cmd r3 tapestry-spike-x264 x264        R3: OBS, x264
  (use tapestry-spike-amf or tapestry-spike-qsv for AMD or Intel encoders)
  After an OBS run, stop streaming and exit OBS at once; fill game_visible and
  audio_present in owner-notes.toml, then: tapestry-spike.exe report runs\<name>

Files: bundle-manifest.json lists every file with its SHA-256; licenses\
holds the FFmpeg (LGPL), PresentMon and MediaMTX (MIT) licenses.
