fn main() {
    let devices = voice_audio::devices().expect("enumerate input devices");
    let capture = voice_audio::Capture::start("default", 1).expect("open default microphone");
    std::thread::sleep(std::time::Duration::from_millis(500));
    let pcm = capture.stop().expect("stop bounded recording");
    assert!(!pcm.is_empty());
    assert!(pcm.iter().all(|sample| sample.is_finite()));
    assert!(pcm.len() <= 16000);
    println!(
        "{}",
        serde_json::json!({"device_count":devices.len(),"pcm_samples_in_memory":pcm.len(),"saved_audio_files":0,"finite_samples":true})
    );
}
