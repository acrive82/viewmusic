//! Linux backend checks. The live-server check is opt-in so CI needs no audio hardware.
#![cfg(target_os = "linux")]

use std::time::{Duration, Instant};

use viz_audio::{tap, CaptureSource, CaptureStage};

#[test]
fn missing_server_returns_actionable_error_without_hanging() {
    // Isolate PulseAudio's environment from other tests and the user's server.
    if std::env::var_os("VIEWMUSIC_TEST_MISSING_SERVER").is_none() {
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "missing_server_returns_actionable_error_without_hanging",
            ])
            .env("VIEWMUSIC_TEST_MISSING_SERVER", "1")
            .env(
                "PULSE_SERVER",
                "unix:/tmp/viewmusic-no-such-audio-server/socket",
            )
            .status()
            .unwrap();
        assert!(result.success());
        return;
    }
    let started = Instant::now();
    let error = match tap::start(Box::new(|_: &[f32]| {})) {
        Ok(_) => panic!("capture unexpectedly connected to a nonexistent server"),
        Err(error) => error,
    };
    assert_eq!(error.stage, CaptureStage::CreateTap);
    assert!(error.message.contains("PulseAudio"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(6));
}

#[test]
#[ignore = "requires a running PipeWire-Pulse or PulseAudio session"]
fn live_capture_delivers_samples_and_stops_promptly() {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let capture = tap::start(Box::new(move |samples: &[f32]| {
        if !samples.is_empty() {
            let _ = tx.try_send(());
        }
    }))
    .unwrap();
    assert!(capture.granted_buffer_frames() > 0);
    rx.recv_timeout(Duration::from_secs(5))
        .expect("no audio buffers received");
    let started = Instant::now();
    drop(capture);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "capture shutdown blocked"
    );
}

/// Uses a private PulseAudio daemon with null sinks: no physical audio, no changes
/// to the desktop server. Set VIEWMUSIC_TEST_PULSEAUDIO to a daemon executable if
/// it is not on PATH; optionally set VIEWMUSIC_TEST_PULSE_MODULES to its module dir.
#[test]
#[ignore = "requires the PulseAudio daemon and pactl/paplay test tools"]
fn isolated_server_follows_outputs_and_recovers_after_restart() {
    use std::process::{Command, Stdio};
    use viz_audio::{AudioPipeline, CaptureState};

    if std::env::var_os("VIEWMUSIC_TEST_ISOLATED_CHILD").is_none() {
        let base = std::env::temp_dir().join(format!("viewmusic-pulse-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let result = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "isolated_server_follows_outputs_and_recovers_after_restart",
                "--ignored",
                "--nocapture",
            ])
            .env("VIEWMUSIC_TEST_ISOLATED_CHILD", &base)
            .env(
                "PULSE_SERVER",
                format!("unix:{}", base.join("pulse.sock").display()),
            )
            .env("PULSE_RUNTIME_PATH", base.join("runtime"))
            .env("PULSE_STATE_PATH", base.join("state"))
            .env("PULSE_CONFIG_PATH", &base)
            .status()
            .unwrap();
        let _ = std::fs::remove_dir_all(&base);
        assert!(result.success(), "isolated audio scenario failed");
        return;
    }

    struct Server {
        child: std::process::Child,
    }
    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
    fn pactl(args: &[&str]) -> String {
        let result = Command::new("pactl").args(args).output().unwrap();
        assert!(
            result.status.success(),
            "pactl {args:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        String::from_utf8(result.stdout).unwrap()
    }
    fn wait_for(description: &str, mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            if condition() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out: {description}");
    }
    fn start_server(base: &std::path::Path) -> Server {
        let daemon =
            std::env::var_os("VIEWMUSIC_TEST_PULSEAUDIO").unwrap_or_else(|| "pulseaudio".into());
        let mut command = Command::new(daemon);
        command
            .args([
                "--daemonize=no",
                "--exit-idle-time=-1",
                "--use-pid-file=no",
                "--high-priority=no",
                "--realtime=no",
                "--disable-shm=yes",
                "-n",
                "--file",
            ])
            .arg(base.join("test.pa"))
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(base.join("daemon.log")).unwrap());
        if let Some(modules) = std::env::var_os("VIEWMUSIC_TEST_PULSE_MODULES") {
            command.arg("--dl-search-path").arg(modules);
        }
        let mut server = Server {
            child: command
                .spawn()
                .expect("PulseAudio daemon executable required"),
        };
        wait_for("private PulseAudio server ready", || {
            assert!(
                server.child.try_wait().unwrap().is_none(),
                "daemon exited: {}",
                std::fs::read_to_string(base.join("daemon.log")).unwrap()
            );
            Command::new("pactl")
                .arg("info")
                .output()
                .unwrap()
                .status
                .success()
        });
        server
    }
    fn captures_monitor(name: &str) -> bool {
        let sources = pactl(&["list", "short", "sources"]);
        let monitor_id = sources.lines().find_map(|line| {
            let mut fields = line.split_whitespace();
            let id = fields.next()?;
            (fields.next()? == name).then_some(id)
        });
        let outputs = pactl(&["list", "short", "source-outputs"]);
        outputs
            .lines()
            .any(|line| line.split_whitespace().nth(1) == monitor_id && monitor_id.is_some())
    }

    let base = std::path::PathBuf::from(std::env::var_os("VIEWMUSIC_TEST_ISOLATED_CHILD").unwrap());
    std::fs::write(
        base.join("test.pa"),
        format!(
            "load-module module-native-protocol-unix socket={} auth-anonymous=1\n\
         load-module module-null-sink sink_name=first rate=44100 channels=2\n\
         load-module module-null-sink sink_name=second rate=48000 channels=1\n\
         set-default-sink first\n",
            base.join("pulse.sock").display()
        ),
    )
    .unwrap();
    let server = start_server(&base);
    let mut audio = AudioPipeline::start().unwrap();
    wait_for("first output monitor captured", || {
        captures_monitor("first.monitor")
    });

    // A real 44.1 kHz stereo WAV exercises the server's conversion to 48 kHz mono.
    let rate = 44100u32;
    let samples = rate * 2;
    let bytes = samples * 2 * 2;
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + bytes).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&rate.to_le_bytes());
    wav.extend_from_slice(&(rate * 4).to_le_bytes());
    wav.extend_from_slice(&4u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&bytes.to_le_bytes());
    for i in 0..samples {
        let sample =
            ((i as f32 * std::f32::consts::TAU * 440.0 / rate as f32).sin() * 12000.0) as i16;
        wav.extend_from_slice(&sample.to_le_bytes());
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    let tone = base.join("tone.wav");
    std::fs::write(&tone, wav).unwrap();
    let mut playback = Command::new("paplay")
        .args(["--device=first", "--latency-msec=10"])
        .arg(&tone)
        .spawn()
        .unwrap();
    wait_for("played signal reaches analyzer", || {
        audio.latest_frame().energy > 0.001
    });
    assert!(playback.wait().unwrap().success());
    println!("played signal captured and analyzed");

    pactl(&["set-default-sink", "second"]);
    wait_for("default output changed to second monitor", || {
        captures_monitor("second.monitor")
    });
    println!("default output change followed");

    drop(server);
    wait_for("outage exposed as capture failure", || {
        matches!(audio.capture_state(), CaptureState::Failed { .. })
    });
    std::thread::sleep(Duration::from_millis(600));
    assert!(
        matches!(audio.capture_state(), CaptureState::Failed { .. }),
        "outage was hidden by historical audio"
    );
    let _server = start_server(&base);
    wait_for("server restart recovers to first monitor", || {
        captures_monitor("first.monitor") && audio.capture_state() == CaptureState::Active
    });
    println!("server restart recovered");

    let modules = pactl(&["list", "short", "modules"]);
    let first = modules
        .lines()
        .find(|line| line.contains("sink_name=first"))
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap();
    pactl(&["unload-module", first]);
    wait_for("removed output falls back to second monitor", || {
        captures_monitor("second.monitor") && audio.capture_state() == CaptureState::Active
    });
    println!("output removal recovered");
    let started = Instant::now();
    audio.stop();
    assert!(started.elapsed() < Duration::from_secs(1));
}
