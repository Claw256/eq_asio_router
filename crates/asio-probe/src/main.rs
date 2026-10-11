//! Console ASIO host for testing drivers.
//!
//!   asio-probe list
//!   asio-probe info "<driver name>"
//!   asio-probe run  "<driver name>" [seconds]   (callback count, no audio)

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use asio_host::{AsioDriver, Handler};
use windows::Win32::Foundation::HWND;

struct Counter(Arc<AtomicU64>);

impl Handler for Counter {
    fn buffer_switch(&mut self, _index: i32) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    asio_host::init_com_sta();
    let bits = if cfg!(target_pointer_width = "32") { "32-bit" } else { "64-bit" };
    match args.get(1).map(String::as_str) {
        Some("list") => {
            println!("ASIO drivers visible to this {bits} process:");
            for d in asio_host::enumerate()? {
                println!("  {:<40} {:?}", d.name, d.clsid);
            }
        }
        Some("info") | Some("run") => {
            let Some(name) = args.get(2) else { bail!("driver name required") };
            let info = asio_host::find_driver(name)?;
            let mut drv = AsioDriver::load_diagnosed(&info, HWND::default())?;
            let (ins, outs) = drv.channels()?;
            let bs = drv.buffer_sizes()?;
            println!("driver      : {} (v{})", drv.driver_name(), drv.driver_version());
            println!("channels    : {ins} in / {outs} out");
            println!("buffer size : min {} max {} preferred {} granularity {}", bs.min, bs.max, bs.preferred, bs.granularity);
            let rate = drv.sample_rate()?;
            println!("sample rate : {rate} Hz");
            let (li, lo) = drv.latencies()?;
            println!("latency     : in {li} / out {lo} samples ({:.2} ms total)", (li + lo) as f64 / rate * 1000.0);
            for i in 0..ins.min(8) {
                let c = drv.channel_info(i, true)?;
                println!("  in  {i}: {} (type {})", c.name, c.sample_type);
            }
            for o in 0..outs.min(8) {
                let c = drv.channel_info(o, false)?;
                println!("  out {o}: {} (type {})", c.name, c.sample_type);
            }
            if args[1] == "run" {
                let secs: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(3);
                let count = Arc::new(AtomicU64::new(0));
                let chans: Vec<(bool, i32)> = (0..outs.min(2)).map(|o| (false, o)).collect();
                drv.create_buffers(&chans, bs.preferred, Box::new(Counter(count.clone())))?;
                drv.start()?;
                let t0 = Instant::now();
                std::thread::sleep(Duration::from_secs(secs));
                drv.stop()?;
                let n = count.load(Ordering::Relaxed);
                let expected = rate * t0.elapsed().as_secs_f64() / bs.preferred as f64;
                println!("callbacks   : {n} in {:.2}s (expected ~{expected:.0})", t0.elapsed().as_secs_f64());
                drv.dispose_buffers()?;
            }
        }
        _ => println!("usage: asio-probe list | info <driver> | run <driver> [seconds]"),
    }
    Ok(())
}
