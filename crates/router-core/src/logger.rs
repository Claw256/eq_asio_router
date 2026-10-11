//! Non-blocking logger: fixed-size records go into a lock-free bounded MPSC ring and a
//! writer thread appends them to `%TEMP%\eq_asio_router_engine.log` (size-capped
//! rotation, default 1 MB, current + one old file).
//!
//! Logging from the audio thread allocates nothing: the message is formatted into a
//! stack buffer (longer messages are truncated) and pushed with a CAS on the ring. If the
//! ring is full the record is dropped and [`Logger::dropped`] is incremented. Before
//! [`Logger::init`] (or after [`Logger::shutdown`]) logging is a no-op.
//!
//! ```ignore
//! router_core::rlog!(router_core::logger::Level::Info, "rate {} Hz", 48000);
//! ```

use std::cell::UnsafeCell;
use std::fmt::{self, Write as _};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Maximum message bytes per record (longer messages are truncated).
pub const MSG_LEN: usize = 192;
/// Ring capacity in records (power of two).
pub const QUEUE_RECORDS: usize = 1024;
/// Default log file size cap.
pub const DEFAULT_MAX_BYTES: u64 = 1_000_000;
/// Log file name inside `%TEMP%`.
pub const LOG_FILE_NAME: &str = "eq_asio_router_engine.log";

/// Severity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
}

impl Level {
    fn name(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => "WARN ",
            Level::Info => "INFO ",
            Level::Debug => "DEBUG",
        }
    }
    fn from_u8(v: u8) -> Level {
        match v {
            0 => Level::Error,
            1 => Level::Warn,
            2 => Level::Info,
            _ => Level::Debug,
        }
    }
}

/// One fixed-size log record.
#[derive(Clone, Copy)]
pub struct Record {
    pub ts_ms: u64,
    pub level: u8,
    pub len: u8,
    pub msg: [u8; MSG_LEN],
}

impl Record {
    const EMPTY: Record = Record { ts_ms: 0, level: 0, len: 0, msg: [0; MSG_LEN] };

    /// Message text (lossy if truncation split a UTF-8 sequence).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.msg[..self.len as usize]).into_owned()
    }
    pub fn level(&self) -> Level {
        Level::from_u8(self.level)
    }
}

struct Slot {
    seq: AtomicUsize,
    data: UnsafeCell<Record>,
}

/// Bounded lock-free MPSC queue (Vyukov sequence-number ring) of [`Record`]s.
pub struct Queue {
    slots: Box<[Slot]>,
    mask: usize,
    enqueue: AtomicUsize,
    dequeue: AtomicUsize,
    dropped: AtomicU64,
}

// SAFETY: slot data is only accessed by the thread that won the slot through the
// sequence-number protocol (producers via CAS on `enqueue`, the single consumer via `dequeue`).
unsafe impl Sync for Queue {}
unsafe impl Send for Queue {}

impl Queue {
    /// Creates a queue of `capacity` records (rounded up to a power of two, minimum 2).
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(2).next_power_of_two();
        Queue {
            slots: (0..cap).map(|i| Slot { seq: AtomicUsize::new(i), data: UnsafeCell::new(Record::EMPTY) }).collect(),
            mask: cap - 1,
            enqueue: AtomicUsize::new(0),
            dequeue: AtomicUsize::new(0),
            dropped: AtomicU64::new(0),
        }
    }

    /// Pushes a record; returns false (and counts a drop) if the queue is full.
    pub fn push(&self, rec: &Record) -> bool {
        let mut pos = self.enqueue.load(Ordering::Relaxed);
        loop {
            let slot = &self.slots[pos & self.mask];
            let seq = slot.seq.load(Ordering::Acquire);
            let dif = seq as isize - pos as isize;
            if dif == 0 {
                match self.enqueue.compare_exchange_weak(pos, pos.wrapping_add(1), Ordering::Relaxed, Ordering::Relaxed) {
                    Ok(_) => {
                        // SAFETY: winning the CAS gives this thread exclusive access until `seq` is published.
                        unsafe { *slot.data.get() = *rec };
                        slot.seq.store(pos.wrapping_add(1), Ordering::Release);
                        return true;
                    }
                    Err(cur) => pos = cur,
                }
            } else if dif < 0 {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                return false;
            } else {
                pos = self.enqueue.load(Ordering::Relaxed);
            }
        }
    }

    /// Pops the oldest record. Single consumer only.
    pub fn pop(&self) -> Option<Record> {
        let pos = self.dequeue.load(Ordering::Relaxed);
        let slot = &self.slots[pos & self.mask];
        let seq = slot.seq.load(Ordering::Acquire);
        if seq as isize - pos.wrapping_add(1) as isize != 0 {
            return None;
        }
        self.dequeue.store(pos.wrapping_add(1), Ordering::Relaxed);
        // SAFETY: the producer published this slot (seq == pos + 1) and no one else reads it.
        let rec = unsafe { *slot.data.get() };
        slot.seq.store(pos.wrapping_add(self.mask + 1), Ordering::Release);
        Some(rec)
    }

    /// Records dropped because the queue was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Stack buffer that truncates instead of failing.
struct FixedBuf {
    buf: [u8; MSG_LEN],
    len: usize,
}

impl fmt::Write for FixedBuf {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let room = MSG_LEN - self.len;
        let k = s.len().min(room);
        self.buf[self.len..self.len + k].copy_from_slice(&s.as_bytes()[..k]);
        self.len += k;
        Ok(())
    }
}

/// Builds a record from format arguments (no allocation).
pub fn make_record(level: Level, args: fmt::Arguments<'_>) -> Record {
    let mut b = FixedBuf { buf: [0; MSG_LEN], len: 0 };
    let _ = b.write_fmt(args);
    let ts_ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    Record { ts_ms, level: level as u8, len: b.len as u8, msg: b.buf }
}

/// Size-capped rotating file: `name`, `name.1`, ... (`keep` files in total).
pub struct RotatingFile {
    path: PathBuf,
    max_bytes: u64,
    keep: usize,
    file: Option<std::fs::File>,
    size: u64,
}

impl RotatingFile {
    /// Opens (appending) `path`. `keep` counts the current file (minimum 1).
    pub fn open(path: impl Into<PathBuf>, max_bytes: u64, keep: usize) -> std::io::Result<Self> {
        let path = path.into();
        let file = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(RotatingFile { path, max_bytes, keep: keep.max(1), file: Some(file), size })
    }

    fn numbered(&self, i: usize) -> PathBuf {
        let mut s = self.path.as_os_str().to_owned();
        s.push(format!(".{i}"));
        PathBuf::from(s)
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        self.file = None;
        if self.keep > 1 {
            for i in (1..self.keep).rev() {
                let from = if i == 1 { self.path.clone() } else { self.numbered(i - 1) };
                let to = self.numbered(i);
                let _ = std::fs::remove_file(&to);
                let _ = std::fs::rename(&from, &to);
            }
        } else {
            let _ = std::fs::remove_file(&self.path);
        }
        self.file = Some(std::fs::OpenOptions::new().create(true).append(true).open(&self.path)?);
        self.size = 0;
        Ok(())
    }

    /// Appends `bytes`, rotating first if the cap would be exceeded.
    pub fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        if self.size > 0 && self.size + bytes.len() as u64 > self.max_bytes {
            self.rotate()?;
        }
        if let Some(f) = self.file.as_mut() {
            f.write_all(bytes)?;
            self.size += bytes.len() as u64;
        }
        Ok(())
    }

    pub fn flush(&mut self) {
        if let Some(f) = self.file.as_mut() {
            let _ = f.flush();
        }
    }
}

static QUEUE: OnceLock<Queue> = OnceLock::new();
static ENABLED: AtomicBool = AtomicBool::new(false);
static STOP: AtomicBool = AtomicBool::new(false);
static WORKER: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);

/// Global logger control.
pub struct Logger;

impl Logger {
    /// Default log path: `%TEMP%\eq_asio_router_engine.log`.
    pub fn default_path() -> PathBuf {
        std::env::temp_dir().join(LOG_FILE_NAME)
    }

    /// Starts the writer thread on the default path (1 MB cap, 2 files). Idempotent.
    pub fn init() -> std::io::Result<()> {
        Self::init_with(Self::default_path(), DEFAULT_MAX_BYTES, 2)
    }

    /// Starts the writer thread on `path`. Does nothing if already running.
    pub fn init_with(path: impl AsRef<Path>, max_bytes: u64, keep: usize) -> std::io::Result<()> {
        let mut w = WORKER.lock().unwrap_or_else(|e| e.into_inner());
        if w.is_some() {
            return Ok(());
        }
        let mut file = RotatingFile::open(path.as_ref(), max_bytes, keep)?;
        let q = QUEUE.get_or_init(|| Queue::new(QUEUE_RECORDS));
        STOP.store(false, Ordering::SeqCst);
        let h = std::thread::Builder::new().name("router-log".into()).spawn(move || {
            let mut reported = 0u64;
            let mut line = String::with_capacity(MSG_LEN + 64);
            loop {
                let stop = STOP.load(Ordering::SeqCst);
                let mut any = false;
                while let Some(rec) = q.pop() {
                    any = true;
                    line.clear();
                    let _ = writeln!(line, "[{}] {} {}", rec.ts_ms, rec.level().name(), rec.text());
                    let _ = file.write(line.as_bytes());
                }
                let d = q.dropped();
                if d != reported {
                    line.clear();
                    let _ = writeln!(line, "[log] {} record(s) dropped (ring full)", d - reported);
                    let _ = file.write(line.as_bytes());
                    reported = d;
                }
                if any {
                    file.flush();
                }
                if stop && !any {
                    break;
                }
                if !any {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
            file.flush();
        })?;
        *w = Some(h);
        ENABLED.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Stops logging, drains the ring to the file and joins the writer thread.
    pub fn shutdown() {
        ENABLED.store(false, Ordering::SeqCst);
        STOP.store(true, Ordering::SeqCst);
        let h = WORKER.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(h) = h {
            let _ = h.join();
        }
    }

    /// Records dropped so far because the ring was full (0 before init).
    pub fn dropped() -> u64 {
        QUEUE.get().map_or(0, Queue::dropped)
    }

    /// True between `init` and `shutdown`.
    pub fn is_running() -> bool {
        ENABLED.load(Ordering::Relaxed)
    }
}

/// Logs a pre-formatted message. Allocation-free; no-op when the logger is not running.
#[inline]
pub fn log(level: Level, args: fmt::Arguments<'_>) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    if let Some(q) = QUEUE.get() {
        q.push(&make_record(level, args));
    }
}

/// `rlog!(Level::Info, "x = {}", x)`: non-blocking, audio-thread-safe log line.
#[macro_export]
macro_rules! rlog {
    ($lvl:expr, $($arg:tt)*) => { $crate::logger::log($lvl, format_args!($($arg)*)) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("router_core_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn record_truncates_long_messages() {
        let long = "x".repeat(1000);
        let r = make_record(Level::Warn, format_args!("{long}"));
        assert_eq!(r.len as usize, MSG_LEN);
        assert_eq!(r.level(), Level::Warn);
        let r = make_record(Level::Info, format_args!("a={} b={:.2}", 1, 2.5));
        assert_eq!(r.text(), "a=1 b=2.50");
    }

    #[test]
    fn queue_fifo_and_drop_count() {
        let q = Queue::new(4);
        for i in 0..6 {
            let ok = q.push(&make_record(Level::Info, format_args!("m{i}")));
            assert_eq!(ok, i < 4);
        }
        assert_eq!(q.dropped(), 2);
        for i in 0..4 {
            assert_eq!(q.pop().unwrap().text(), format!("m{i}"));
        }
        assert!(q.pop().is_none());
        // wraps and works again
        for round in 0..20 {
            assert!(q.push(&make_record(Level::Info, format_args!("r{round}"))));
            assert_eq!(q.pop().unwrap().text(), format!("r{round}"));
        }
    }

    #[test]
    fn queue_multi_producer_integrity() {
        let q = Arc::new(Queue::new(64));
        let n_threads = 4;
        let per = 5000;
        let mut hs = Vec::new();
        for t in 0..n_threads {
            let q = q.clone();
            hs.push(std::thread::spawn(move || {
                let mut pushed = 0;
                for i in 0..per {
                    while !q.push(&make_record(Level::Debug, format_args!("{t}:{i}"))) {
                        std::thread::yield_now();
                    }
                    pushed += 1;
                }
                pushed
            }));
        }
        let mut last = vec![-1i64; n_threads];
        let mut got = 0;
        while got < n_threads * per {
            if let Some(r) = q.pop() {
                let text = r.text();
                let (t, i) = text.split_once(':').unwrap();
                let (t, i): (usize, i64) = (t.parse().unwrap(), i.parse().unwrap());
                assert_eq!(i, last[t] + 1, "per-producer order preserved");
                last[t] = i;
                got += 1;
            } else {
                std::thread::yield_now();
            }
        }
        for h in hs {
            h.join().unwrap();
        }
    }

    #[test]
    fn rotation_caps_size_and_keeps_two_files() {
        let d = tmp("rot");
        let p = d.join("x.log");
        let mut f = RotatingFile::open(&p, 100, 2).unwrap();
        for i in 0..30 {
            f.write(format!("line number {i:04}\n").as_bytes()).unwrap();
        }
        f.flush();
        let cur = std::fs::metadata(&p).unwrap().len();
        let old = std::fs::metadata(d.join("x.log.1")).unwrap().len();
        assert!(cur <= 100 && old <= 100, "{cur} {old}");
        assert!(!d.join("x.log.2").exists());
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("line number 0029"));
        // keep = 3 -> two old files
        let p3 = d.join("y.log");
        let mut f = RotatingFile::open(&p3, 50, 3).unwrap();
        for i in 0..30 {
            f.write(format!("line number {i:04}\n").as_bytes()).unwrap();
        }
        assert!(d.join("y.log.2").exists() && !d.join("y.log.3").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Single test touching the global logger (state is process-wide).
    #[test]
    fn global_logger_end_to_end() {
        let d = tmp("glob");
        let p = d.join("engine.log");
        // not running yet: no-op
        rlog!(Level::Info, "ignored");
        Logger::init_with(&p, 10_000, 2).unwrap();
        assert!(Logger::is_running());
        Logger::init_with(&p, 10_000, 2).unwrap(); // idempotent
        let h = std::thread::spawn(|| {
            for i in 0..50 {
                rlog!(Level::Warn, "from thread {i}");
            }
        });
        rlog!(Level::Info, "hello {}", 42);
        h.join().unwrap();
        Logger::shutdown();
        assert!(!Logger::is_running());
        rlog!(Level::Info, "after shutdown");
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("INFO  hello 42"), "{text}");
        assert!(text.contains("from thread 49"));
        assert!(!text.contains("ignored") && !text.contains("after shutdown"));
        // restart works
        Logger::init_with(&p, 10_000, 2).unwrap();
        rlog!(Level::Error, "second run");
        Logger::shutdown();
        assert!(std::fs::read_to_string(&p).unwrap().contains("ERROR second run"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
