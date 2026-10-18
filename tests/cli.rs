//! End-to-end tests through the `ferrompeg` binary.

use std::path::{Path, PathBuf};
use std::process::Command;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ferrompeg-cli-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run(args: &[&str]) {
    let out = Command::new(env!("CARGO_BIN_EXE_ferrompeg"))
        .args(["-hide_banner", "-nostats", "-v", "error", "-y"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "ferrompeg {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn probe_json(path: &Path) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_ferroprobe"))
        .args([
            "-hide_banner",
            "-v",
            "error",
            "-show_streams",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap()
}

/// Sample data of a canonical 44-byte header WAV written by ferrompeg.
fn wav_samples(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap()[44..].to_vec()
}

#[test]
fn flac_round_trip_is_lossless() {
    let dir = scratch("flac");
    let wav = dir.join("a.wav");
    let flac = dir.join("a.flac");
    let back = dir.join("b.wav");
    run(&[
        "-f",
        "lavfi",
        "-i",
        "anoisesrc=d=1:c=pink:a=0.5",
        "-ac",
        "2",
        wav.to_str().unwrap(),
    ]);
    run(&["-i", wav.to_str().unwrap(), flac.to_str().unwrap()]);
    run(&["-i", flac.to_str().unwrap(), back.to_str().unwrap()]);
    assert_eq!(wav_samples(&wav), wav_samples(&back));
    assert!(std::fs::metadata(&flac).unwrap().len() < std::fs::metadata(&wav).unwrap().len());
}

#[test]
fn trim_keeps_samples_buffered_in_filters() {
    let dir = scratch("trim");
    let out = dir.join("r.wav");
    run(&[
        "-f",
        "lavfi",
        "-i",
        "sine=f=1000:d=2",
        "-t",
        "1",
        "-af",
        "aresample=48000",
        out.to_str().unwrap(),
    ]);
    assert!(probe_json(&out).contains("\"duration_ts\": 48000"));
}

#[test]
fn video_pipeline_to_gif_and_y4m() {
    let dir = scratch("video");
    let gif = dir.join("t.gif");
    let y4m = dir.join("t.y4m");
    run(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc=d=1:r=10",
        "-vf",
        "scale=160:-2",
        gif.to_str().unwrap(),
    ]);
    let bytes = std::fs::read(&gif).unwrap();
    assert!(bytes.starts_with(b"GIF89a"));
    assert_eq!(bytes.last(), Some(&0x3B));
    run(&[
        "-f",
        "lavfi",
        "-i",
        "smptebars=d=1:r=5",
        "-vf",
        "crop=100:50",
        y4m.to_str().unwrap(),
    ]);
    let json = probe_json(&y4m);
    assert!(json.contains("\"width\": 100") && json.contains("\"height\": 50"));
    let frames = std::fs::read(&y4m)
        .unwrap()
        .windows(6)
        .filter(|w| w == b"FRAME\n")
        .count();
    assert_eq!(frames, 5);
}

#[test]
fn stream_copy_preserves_bytes() {
    let dir = scratch("copy");
    let a = dir.join("a.wav");
    let b = dir.join("b.wav");
    run(&["-f", "lavfi", "-i", "sine=d=0.5", a.to_str().unwrap()]);
    run(&["-i", a.to_str().unwrap(), "-c", "copy", b.to_str().unwrap()]);
    assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
}

#[test]
fn output_does_not_depend_on_the_thread_count() {
    let dir = scratch("threads");
    let p = |name: &str| dir.join(name).to_str().unwrap().to_string();
    // Long enough for several FLAC and GIF batches and many picture bands.
    run(&[
        "-f",
        "lavfi",
        "-i",
        "anoisesrc=d=8:c=pink:a=0.5",
        "-ac",
        "2",
        &p("in.wav"),
    ]);
    run(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc=s=640x360:r=25:d=3",
        &p("in.y4m"),
    ]);
    let cases: [(&str, &[&str], &str); 5] = [
        ("in.wav", &[], "flac"),
        ("in.wav", &["-ar", "48000", "-af", "atempo=1.3"], "wav"),
        ("in.y4m", &["-vf", "scale=500:-2:flags=bicubic"], "y4m"),
        ("in.y4m", &["-pix_fmt", "rgb24", "-f", "rawvideo"], "rgb"),
        ("in.y4m", &["-vf", "scale=320:-2,fps=10"], "gif"),
    ];
    for (i, (input, opts, ext)) in cases.iter().enumerate() {
        let mut outputs = Vec::new();
        for threads in ["1", "3", "8"] {
            let out = p(&format!("{i}-{threads}.{ext}"));
            let mut args = vec!["-threads", threads, "-i"];
            let input = p(input);
            args.push(&input);
            args.extend_from_slice(opts);
            args.push(&out);
            run(&args);
            outputs.push(std::fs::read(&out).unwrap());
        }
        assert!(
            outputs.windows(2).all(|w| w[0] == w[1]),
            "case {i} ({ext}) differs between thread counts"
        );
    }
    // Decoding the FLAC is batched too.
    run(&["-threads", "1", "-i", &p("0-1.flac"), &p("d1.wav")]);
    run(&["-threads", "8", "-i", &p("0-1.flac"), &p("d8.wav")]);
    assert_eq!(
        wav_samples(&dir.join("d1.wav")),
        wav_samples(&dir.join("in.wav"))
    );
    assert_eq!(
        wav_samples(&dir.join("d8.wav")),
        wav_samples(&dir.join("in.wav"))
    );
}
