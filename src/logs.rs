// Live logs: every formatted log line goes to stdout, a 1000-line ring buffer and a broadcast channel.
// Each WebSocket reader consumes at its own pace; a lagging one is told how many lines it missed.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;
use tokio::sync::broadcast;

const HISTORY: usize = 1000;

pub struct Logs {
    ring: Mutex<VecDeque<Arc<str>>>,
    tx: broadcast::Sender<Arc<str>>,
}

static LOGS: OnceLock<Logs> = OnceLock::new();

pub fn logs() -> &'static Logs {
    LOGS.get_or_init(|| Logs {
        ring: Mutex::new(VecDeque::with_capacity(HISTORY)),
        tx: broadcast::channel(1024).0,
    })
}

impl Logs {
    pub fn push(&self, line: &str) {
        let line: Arc<str> = Arc::from(line.trim_end());
        if line.is_empty() {
            return;
        }
        let mut ring = self.ring.lock();
        if ring.len() == HISTORY {
            ring.pop_front();
        }
        ring.push_back(line.clone());
        let _ = self.tx.send(line);
    }

    /// The history and a receiver for what follows, taken together so no line is lost or doubled.
    pub fn subscribe(&self) -> (Vec<Arc<str>>, broadcast::Receiver<Arc<str>>) {
        let ring = self.ring.lock();
        (ring.iter().cloned().collect(), self.tx.subscribe())
    }
}

/// `MakeWriter` for tracing-subscriber: tees each formatted event to stdout and the log buffer.
#[derive(Clone, Copy, Default)]
pub struct Tee;

pub struct TeeWriter(Vec<u8>);

impl Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for TeeWriter {
    fn drop(&mut self) {
        let _ = io::stdout().write_all(&self.0);
        let text = String::from_utf8_lossy(&self.0);
        for line in text.lines() {
            logs().push(line);
        }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Tee {
    type Writer = TeeWriter;

    fn make_writer(&'a self) -> Self::Writer {
        TeeWriter(Vec::with_capacity(256))
    }
}

pub fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::fmt::time::ChronoLocal;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_timer(ChronoLocal::new("%Y/%m/%d %H:%M:%S".into()))
        .with_target(false)
        .with_ansi(false)
        .with_writer(Tee)
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_keeps_the_last_lines_and_subscribers_see_new_ones() {
        let logs = Logs {
            ring: Mutex::new(VecDeque::new()),
            tx: broadcast::channel(4).0,
        };
        for i in 0..1005 {
            logs.push(&format!("line {i}\n"));
        }
        let (history, mut rx) = logs.subscribe();
        assert_eq!(history.len(), HISTORY);
        assert_eq!(&*history[0], "line 5");
        logs.push("next");
        assert_eq!(&*rx.try_recv().unwrap(), "next");
    }
}
