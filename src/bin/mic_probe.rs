//! Test: `mic_probe [seconds]` puts "Mac Microphone" on Windows with a 440 Hz tone, prints whether
//! something records from it each second, and unplugs it at the end.
fn main() {
    let seconds: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(30);
    let mic = match mac_microphone::Mic::start("Mac Microphone", "probe") {
        Ok(mic) => mic,
        Err(error) => {
            eprintln!("start failed: {error}");
            std::process::exit(1);
        }
    };
    mic.set_tone(true);
    println!("attached");
    for second in 0..seconds {
        std::thread::sleep(std::time::Duration::from_secs(1));
        println!("t={second} in_use={} stats={:?}", mic.feed().in_use(), mic.feed().stats());
    }
    drop(mic);
    println!("detached");
}
