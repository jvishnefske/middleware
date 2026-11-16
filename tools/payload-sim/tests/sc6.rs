//! Runs the SC6 sensor fixture (shared with the browser simulator's tests)
//! in `payload-sim`: a payload streaming the fake IMU and the fake
//! microphone, one batch line per published batch, until the payload
//! stops its IMU route.

use std::path::PathBuf;
use std::process::Command;

const SENSOR_STREAM: &str = include_str!("../../../web/test/fixtures/sensor_stream.wat");

#[test]
fn sc6_payload_sim_streams_the_fake_imu_and_microphone() {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("sc6_sensor_stream.wasm");
    std::fs::write(&path, wat::parse_str(SENSOR_STREAM).expect("valid WAT")).expect("write wasm");
    let out = Command::new(env!("CARGO_BIN_EXE_payload-sim"))
        .arg(&path)
        .args(["--virtual", "--until-ms", "500"])
        .output()
        .expect("payload-sim runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}\n{}", String::from_utf8_lossy(&out.stderr));
    let has = |needle: &str| stdout.lines().any(|l| l.contains(needle));
    assert!(has("route0 source Imu 5 per batch every 100000 us on app/imu"), "{stdout}");
    assert!(has("route1 source Microphone 2 per batch every 100000 us on app/mic"), "{stdout}");
    // 32-byte header + 5 samples x 3 channels x 2 bytes; 32 + 2 blocks x 4 bytes.
    assert!(has("route0 batch 0 -> app/imu (62 B)"), "{stdout}");
    assert!(has("route1 batch 0 -> app/mic (40 B)"), "{stdout}");
    assert!(has("route0 batch 2 -> app/imu"), "{stdout}");
    assert!(!has("route0 batch 3"), "the payload stopped the IMU: {stdout}");
    assert!(has("route0 stop"), "{stdout}");
    assert!(has("route1 batch 4 -> app/mic"), "{stdout}");
}
