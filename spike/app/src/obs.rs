//! OBS profiles and scene collection for the OBS runs (design §4.1's fixed
//! settings), written by `obs-files --out <dir>` so the scene collection
//! can name the overlay page by its absolute path in the unzipped bundle.
//! The owner imports them in OBS (Profile > Import, Scene Collection >
//! Import) once per PC. The canvas (base resolution) is the game's native
//! resolution (`--canvas WxH`, or the mode of `--monitor`); the game and
//! the overlay are fitted to it, and OBS scales the canvas to the 1920×1080
//! output.
//!
//! Profiles (one per encoder family, H.264 CBR 6000 kbps, keyframe 2 s,
//! Advanced output, no encoder-side rescale, recording not started):
//! `tapestry-spike-nvenc` (P5, HQ, two-pass quarter resolution, high),
//! `tapestry-spike-amf` (quality, high), `tapestry-spike-qsv` (balanced
//! TU4, high), `tapestry-spike-x264` (veryfast, high, no tune). Output
//! 1920×1080 at 60 fps, bicubic; audio 48 kHz stereo AAC 160 kbps; stream
//! to the bundle's MediaMTX at rtmp://127.0.0.1:1935/live.
//!
//! Scene collection `tapestry-spike`: game capture of the spike's window
//! (cursor off, the game's own audio captured with it, since the collection
//! has no desktop audio source) and one local browser source
//! (`overlay.html`, 1920×1080 at 30 fps).

use serde_json::{Value, json};
use std::path::Path;

pub const COLLECTION: &str = "tapestry-spike";
pub const SERVER: &str = "rtmp://127.0.0.1:1935/live";
pub const STREAM_KEY: &str = "spike";

/// (profile name, OBS encoder id, encoder settings).
pub fn profiles() -> Vec<(&'static str, &'static str, Value)> {
    let common = |extra: Value| {
        let mut v =
            json!({"rate_control": "CBR", "bitrate": 6000, "keyint_sec": 2, "profile": "high"});
        if let (Some(m), Some(e)) = (v.as_object_mut(), extra.as_object()) {
            m.extend(e.clone());
        }
        v
    };
    vec![
        (
            "tapestry-spike-nvenc",
            "obs_nvenc_h264_tex",
            common(json!({"preset": "p5", "tune": "hq", "multipass": "qres"})),
        ),
        (
            "tapestry-spike-amf",
            "h264_texture_amf",
            common(json!({"preset": "quality"})),
        ),
        (
            "tapestry-spike-qsv",
            "obs_qsv11_v2",
            common(json!({"target_usage": "TU4"})),
        ),
        (
            "tapestry-spike-x264",
            "obs_x264",
            common(json!({"preset": "veryfast", "tune": ""})),
        ),
    ]
}

/// Settings keys each encoder reads in OBS 33 (its properties); a key
/// outside these is ignored by OBS and leaves the default in place.
#[cfg(test)]
fn encoder_keys(encoder: &str) -> &'static [&'static str] {
    match encoder {
        "obs_nvenc_h264_tex" => &[
            "rate_control",
            "bitrate",
            "keyint_sec",
            "preset",
            "tune",
            "multipass",
            "profile",
        ],
        "h264_texture_amf" => &["rate_control", "bitrate", "keyint_sec", "preset", "profile"],
        "obs_qsv11_v2" => &[
            "rate_control",
            "bitrate",
            "keyint_sec",
            "target_usage",
            "profile",
        ],
        "obs_x264" => &[
            "rate_control",
            "bitrate",
            "keyint_sec",
            "preset",
            "profile",
            "tune",
        ],
        _ => &[],
    }
}

fn basic_ini(name: &str, encoder: &str, (cw, ch): (u32, u32)) -> String {
    format!(
        "[General]
Name={name}

[Output]
Mode=Advanced

[AdvOut]
ApplyServiceSettings=false
UseRescale=false
TrackIndex=1
Encoder={encoder}
AudioEncoder=ffmpeg_aac
Track1Bitrate=160
RecType=Standard

[Video]
BaseCX={cw}
BaseCY={ch}
OutputCX=1920
OutputCY=1080
FPSType=0
FPSCommon=60
ScaleType=bicubic
ColorFormat=NV12
ColorSpace=709
ColorRange=Partial

[Audio]
SampleRate=48000
ChannelSetup=Stereo
"
    )
}

/// The scene collection for a `canvas` (width, height): both items are
/// scaled to fit the whole canvas.
pub fn scene_collection(overlay: &Path, (cw, ch): (u32, u32)) -> Value {
    let overlay = overlay.to_string_lossy().replace('\\', "/");
    let fit = json!({"x": f64::from(cw), "y": f64::from(ch)});
    json!({
        "name": COLLECTION,
        "current_scene": "spike",
        "current_program_scene": "spike",
        "scene_order": [{"name": "spike"}],
        "groups": [],
        "quick_transitions": [],
        "transitions": [],
        "sources": [
            {
                "id": "scene",
                "versioned_id": "scene",
                "name": "spike",
                "settings": {
                    "id_counter": 2,
                    "items": [
                        {
                            "name": "game",
                            "id": 1,
                            "visible": true,
                            "locked": false,
                            "pos": {"x": 0.0, "y": 0.0},
                            "scale": {"x": 1.0, "y": 1.0},
                            "align": 5,
                            "bounds_type": 2,
                            "bounds_align": 0,
                            "bounds": fit
                        },
                        {
                            "name": "overlay",
                            "id": 2,
                            "visible": true,
                            "locked": false,
                            "pos": {"x": 0.0, "y": 0.0},
                            "scale": {"x": 1.0, "y": 1.0},
                            "align": 5,
                            "bounds_type": 2,
                            "bounds_align": 0,
                            "bounds": fit
                        }
                    ]
                }
            },
            {
                "id": "game_capture",
                "versioned_id": "game_capture",
                "name": "game",
                "settings": {
                    "capture_mode": "window",
                    "window": "tapestry-spike:Window Class:tapestry-spike.exe",
                    "priority": 2,
                    "capture_cursor": false,
                    "capture_audio": true
                }
            },
            {
                "id": "browser_source",
                "versioned_id": "browser_source",
                "name": "overlay",
                "settings": {
                    "is_local_file": true,
                    "local_file": overlay,
                    "width": 1920,
                    "height": 1080,
                    "fps_custom": true,
                    "fps": 30,
                    "shutdown": false,
                    "restart_when_active": false
                }
            }
        ]
    })
}

/// `obs-files --out <dir> [--overlay <html>]` for a `canvas`.
pub fn write(out: &Path, overlay: &Path, canvas: (u32, u32)) -> Result<(), String> {
    let overlay = std::path::absolute(overlay).map_err(|e| e.to_string())?;
    let io = |e: std::io::Error| e.to_string();
    for (name, encoder, settings) in profiles() {
        let dir = out.join("profiles").join(name);
        std::fs::create_dir_all(&dir).map_err(io)?;
        std::fs::write(dir.join("basic.ini"), basic_ini(name, encoder, canvas)).map_err(io)?;
        std::fs::write(dir.join("streamEncoder.json"), settings.to_string()).map_err(io)?;
        let service = json!({"type": "rtmp_custom", "settings": {
            "server": SERVER, "key": STREAM_KEY, "use_auth": false, "bwtest": false}});
        std::fs::write(dir.join("service.json"), service.to_string()).map_err(io)?;
    }
    let scenes = out.join("scenes");
    std::fs::create_dir_all(&scenes).map_err(io)?;
    std::fs::write(
        scenes.join(format!("{COLLECTION}.json")),
        serde_json::to_vec_pretty(&scene_collection(&overlay, canvas))
            .map_err(|e| e.to_string())?,
    )
    .map_err(io)?;
    println!(
        "obs-files: profiles and scene collection for a {}x{} canvas in {} (import both in OBS)",
        canvas.0,
        canvas.1,
        out.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use spike_core::obslog::{EncoderFamily, ObsEncoderSettings};

    // The profiles carry the workload V7 checks in the log (§4.1).
    #[test]
    fn profiles_match_the_procedure_settings() {
        for (name, encoder, s) in profiles() {
            // Only keys OBS reads for this encoder (an unread key would
            // leave OBS's default in place unnoticed).
            for k in s.as_object().unwrap().keys() {
                assert!(encoder_keys(encoder).contains(&k.as_str()), "{name}: {k}");
            }
            let get = |k: &str| s.get(k).and_then(Value::as_str).map(str::to_owned);
            let family = match name {
                "tapestry-spike-nvenc" => EncoderFamily::Nvenc,
                "tapestry-spike-amf" => EncoderFamily::Amf,
                "tapestry-spike-qsv" => EncoderFamily::Qsv,
                _ => EncoderFamily::X264,
            };
            let e = ObsEncoderSettings {
                family: Some(family),
                label: Some("texture-amf-h264".into()),
                codec: Some("h264".into()),
                rate_control: get("rate_control"),
                bitrate_kbps: s.get("bitrate").and_then(Value::as_u64).map(|b| b as u32),
                preset: get("preset"),
                profile: get("profile"),
                tune: get("tune").filter(|_| family == EncoderFamily::X264),
                tuning: get("tune").filter(|_| family == EncoderFamily::Nvenc),
                multipass: get("multipass"),
                target_usage: get("target_usage"),
                width: Some(1920),
                height: Some(1080),
            };
            assert!(e.matches_procedure(), "{name}: {e:?}");
        }
    }

    #[test]
    fn game_and_overlay_fill_the_canvas_at_any_native_resolution() {
        for canvas in [(1920, 1080), (2560, 1440), (3840, 2160)] {
            let c = scene_collection(Path::new("C:/bundle/obs/overlay.html"), canvas);
            let items = c["sources"][0]["settings"]["items"].as_array().unwrap();
            for item in items {
                assert_eq!(item["bounds_type"], 2, "{item}");
                assert_eq!(item["bounds"]["x"], f64::from(canvas.0));
                assert_eq!(item["bounds"]["y"], f64::from(canvas.1));
                assert_eq!(
                    (item["pos"]["x"].as_f64(), item["pos"]["y"].as_f64()),
                    (Some(0.0), Some(0.0))
                );
            }
            let ini = basic_ini("p", "obs_x264", canvas);
            assert!(ini.contains(&format!("BaseCX={}\nBaseCY={}\n", canvas.0, canvas.1)));
            assert!(ini.contains("OutputCX=1920\nOutputCY=1080\n"));
        }
    }

    #[test]
    fn game_capture_captures_the_game_audio() {
        let c = scene_collection(Path::new("o.html"), (1920, 1080));
        let game = &c["sources"][1];
        assert_eq!(game["id"], "game_capture");
        assert_eq!(game["settings"]["capture_audio"], true);
        // No other audio source is defined, so the game's own audio is the
        // stream's audio.
        let sources = c["sources"].as_array().unwrap();
        assert!(
            sources
                .iter()
                .all(|s| !s["id"].as_str().unwrap().contains("wasapi"))
        );
    }
}
