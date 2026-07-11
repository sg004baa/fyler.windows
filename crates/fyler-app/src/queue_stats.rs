//! app層のunbounded channel滞留量を観測する軽量カウンター。

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, mpsc};

/// チャネル滞留数とhigh-water markを追跡する軽量ゲージ。
#[derive(Debug, Default)]
pub struct QueueGauge {
    depth: AtomicI64,
    high_water: AtomicI64,
}

impl QueueGauge {
    /// 空のゲージを作成する。
    pub fn new() -> Self {
        Self::default()
    }

    /// 受信側が1件を消費したことを記録する。
    pub fn dequeue(&self) {
        let previous = self.depth.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "queue depth must not become negative");
    }

    /// 現在の滞留数を返す。
    #[cfg(test)]
    pub fn depth(&self) -> i64 {
        self.depth.load(Ordering::Acquire)
    }

    /// 観測した最大滞留数を返す。
    pub fn high_water(&self) -> i64 {
        self.high_water.load(Ordering::Acquire)
    }

    fn enqueue(&self) {
        let depth = self.depth.fetch_add(1, Ordering::AcqRel) + 1;
        self.high_water.fetch_max(depth, Ordering::AcqRel);
    }
}

/// 送信成功時に滞留数とhigh-water markを更新するsender。
pub struct CountingSender<T> {
    inner: mpsc::Sender<T>,
    gauge: Arc<QueueGauge>,
}

impl<T> CountingSender<T> {
    /// 標準senderと共有ゲージを組み合わせる。
    pub fn new(inner: mpsc::Sender<T>, gauge: Arc<QueueGauge>) -> Self {
        Self { inner, gauge }
    }

    /// 値を送信し、成功した送信だけを滞留数へ反映する。
    pub fn send(&self, value: T) -> Result<(), mpsc::SendError<T>> {
        // 受信が送信直後に走ってもdepthが負にならないよう、先に予約する。
        self.gauge.enqueue();
        match self.inner.send(value) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.gauge.dequeue();
                Err(error)
            }
        }
    }
}

impl<T> Clone for CountingSender<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            gauge: Arc::clone(&self.gauge),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enqueue_dequeue_tracks_depth_and_high_water() {
        let gauge = Arc::new(QueueGauge::new());
        let (tx, rx) = mpsc::channel();
        let tx = CountingSender::new(tx, Arc::clone(&gauge));

        tx.send(1).unwrap();
        tx.send(2).unwrap();
        assert_eq!(gauge.depth(), 2);
        assert_eq!(gauge.high_water(), 2);

        assert_eq!(rx.recv().unwrap(), 1);
        gauge.dequeue();
        assert_eq!(gauge.depth(), 1);
        assert_eq!(gauge.high_water(), 2);
    }

    #[test]
    fn concurrent_sends_keep_high_water_monotonic() {
        let gauge = Arc::new(QueueGauge::new());
        let (tx, rx) = mpsc::channel();
        let tx = CountingSender::new(tx, Arc::clone(&gauge));
        let workers = (0..4)
            .map(|worker| {
                let tx = tx.clone();
                std::thread::spawn(move || {
                    for item in 0..250 {
                        tx.send((worker, item)).unwrap();
                    }
                })
            })
            .collect::<Vec<_>>();

        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(gauge.depth(), 1_000);
        assert_eq!(gauge.high_water(), 1_000);

        for _ in 0..1_000 {
            rx.recv().unwrap();
            gauge.dequeue();
            assert_eq!(gauge.high_water(), 1_000);
        }
        assert_eq!(gauge.depth(), 0);
    }
}
