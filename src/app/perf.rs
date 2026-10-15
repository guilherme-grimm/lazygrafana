//! Lightweight runtime statistics: frame times, bytes written to the
//! terminal, query latencies, image encoding. Shown with `:stats`.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Stdout wrapper that counts bytes, so we can see what images cost on the wire.
pub struct CountingStdout {
    inner: io::Stdout,
    bytes: Arc<AtomicU64>,
}

impl CountingStdout {
    pub fn new(bytes: Arc<AtomicU64>) -> Self {
        Self { inner: io::stdout(), bytes }
    }
}

impl Write for CountingStdout {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.bytes.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

const KEEP: usize = 500;

#[derive(Default)]
pub struct Perf {
    pub frames: VecDeque<(Duration, u64)>,
    pub queries: VecDeque<Duration>,
    pub points: VecDeque<usize>,
    pub encodes: VecDeque<Duration>,
    pub bytes_total: Arc<AtomicU64>,
    last_bytes: u64,
    pub in_flight: Arc<std::sync::atomic::AtomicUsize>,
}

fn push<T>(q: &mut VecDeque<T>, v: T) {
    if q.len() == KEEP {
        q.pop_front();
    }
    q.push_back(v);
}

impl Perf {
    /// Records a frame; returns the bytes it wrote.
    pub fn frame(&mut self, took: Duration) -> u64 {
        let total = self.bytes_total.load(Ordering::Relaxed);
        let bytes = total - self.last_bytes;
        self.last_bytes = total;
        push(&mut self.frames, (took, bytes));
        if took > Duration::from_millis(50) {
            tracing::debug!(ms = took.as_millis() as u64, bytes, "slow frame");
        }
        bytes
    }

    pub fn query(&mut self, took: Duration) {
        push(&mut self.queries, took);
        tracing::debug!(ms = took.as_millis() as u64, "query");
    }

    pub fn points(&mut self, n: usize) {
        push(&mut self.points, n);
    }

    pub fn encode(&mut self, took: Duration) {
        push(&mut self.encodes, took);
    }

    pub fn report(&self) -> Vec<String> {
        let mut out = Vec::new();
        let frames: Vec<Duration> = self.frames.iter().map(|(d, _)| *d).collect();
        let bytes: Vec<u64> = self.frames.iter().map(|(_, b)| *b).collect();
        out.push(format!("frames (last {})      {}", frames.len(), durations(&frames)));
        out.push(format!(
            "bytes/frame           avg {}  max {}  total {}",
            human_bytes(bytes.iter().sum::<u64>() / bytes.len().max(1) as u64),
            human_bytes(bytes.iter().copied().max().unwrap_or(0)),
            human_bytes(self.bytes_total.load(Ordering::Relaxed))
        ));
        let q: Vec<Duration> = self.queries.iter().copied().collect();
        let pts: Vec<usize> = self.points.iter().copied().collect();
        out.push(format!("queries (last {})     {}", q.len(), durations(&q)));
        out.push(format!(
            "points/query          avg {}  max {}",
            pts.iter().sum::<usize>() / pts.len().max(1),
            pts.iter().copied().max().unwrap_or(0)
        ));
        out.push(format!("queries in flight     {}", self.in_flight.load(Ordering::Relaxed)));
        let e: Vec<Duration> = self.encodes.iter().copied().collect();
        out.push(format!("image encodes ({})    {}", e.len(), durations(&e)));
        if let Some(rss) = rss_bytes() {
            out.push(format!("memory (RSS)          {}", human_bytes(rss)));
        }
        out
    }
}

pub fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx]
}

fn durations(v: &[Duration]) -> String {
    let mut s = v.to_vec();
    s.sort();
    let ms = |d: Duration| format!("{:.1}ms", d.as_secs_f64() * 1000.0);
    format!("p50 {}  p95 {}  max {}", ms(percentile(&s, 0.5)), ms(percentile(&s, 0.95)), ms(s.last().copied().unwrap_or_default()))
}

pub fn human_bytes(b: u64) -> String {
    match b {
        b if b >= 1 << 30 => format!("{:.1} GiB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MiB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.1} KiB", b as f64 / 1024.0),
        b => format!("{b} B"),
    }
}

/// Resident memory from /proc (Linux only).
pub fn rss_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * 4096)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats() {
        let mut p = Perf::default();
        p.bytes_total.store(1000, Ordering::Relaxed);
        assert_eq!(p.frame(Duration::from_millis(5)), 1000);
        p.bytes_total.store(1500, Ordering::Relaxed);
        assert_eq!(p.frame(Duration::from_millis(15)), 500);
        p.query(Duration::from_millis(100));
        p.points(3000);
        let r = p.report().join("\n");
        assert!(r.contains("p95") && r.contains("1.5 KiB"), "{r}");
        assert_eq!(human_bytes(3 << 20), "3.0 MiB");
    }
}
