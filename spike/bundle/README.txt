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
record the per-program power management setting that applies to this
copy of tapestry-spike.exe (default, normal, optimal-power, adaptive,
prefer-max-performance, prefer-consistent-performance). The driver's
program profile is per executable path: it must point at the
tapestry-spike.exe in this folder, not another copy; without such a
profile, declare default. The spike cannot read the setting; it records
the declaration and the GPU's observed P-state and clocks during play, and
the report warns when prefer-max-performance is declared but the GPU never
reached P0.

Runs (results in runs\<name>; hand off only runs\<name>\export\*.zip)
  run-starve.cmd s0                         S0: prints "S0: pass" or the failure
  run-baseline.cmd r1                       R1: standard, 600 s
  run-obs.cmd r2 tapestry-spike-nvenc hardware   R2: OBS, hardware encoder
  run-obs.cmd r3 tapestry-spike-x264 x264        R3: OBS, x264
  (use tapestry-spike-amf or tapestry-spike-qsv for AMD or Intel encoders)
  After an OBS run, stop streaming and exit OBS at once; answer game_visible
  and audio_present in runs\<name>\owner-review.toml when asked (each OBS run
  has its own; answers never carry over to another run); the report
  follows. After a gate failure the scripts print the next run (streaming
  profile, or the next audio period).

Unattended sessions (design 4.3; nobody at the PC): prepare the PC once
(settings as above, other windows closed or minimized off the measured
monitor, display-off and sleep never on AC), enable OBS's WebSocket server
(Tools > WebSocket Server Settings, authentication on), and use
run-obs-unattended.cmd instead of run-obs.cmd. It stops streaming and closes
OBS by itself and records the ingest to runs\<name>\private\ingest\. It never
kills OBS: if OBS cannot be closed through the WebSocket server (or another
OBS is running), the run fails and OBS is left running for you. Review each
recording later and answer in that run's runs\<name>\owner-review.toml:
  1. Watch runs\<name>\private\ingest\live\spike\*.mp4.
  2. Set game_visible and audio_present to true or false (also note
     hitches, tearing, a stuck BGA or crackles for the hand-off).
  3. Rerun: tapestry-spike.exe report runs\<name>
Until both are answered the OBS result is provisional (exit 4), never a
pass; a false makes it invalid. S0 and R1 run unattended as they are.

Files: bundle-manifest.json lists every file with its SHA-256; licenses\
holds the FFmpeg (LGPL), PresentMon and MediaMTX (MIT) licenses.
