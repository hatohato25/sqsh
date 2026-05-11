/// パフォーマンス計測モジュール
///
/// Phase 3.2: レイテンシ計測機能
///
/// 各操作の実行時間を計測し、ログに出力する
use std::time::{Duration, Instant};

/// 操作の実行時間を計測するマクロ
///
/// 使い方:
/// ```ignore
/// use sqsh::measure_latency;
///
/// measure_latency!("query_execution", {
///     execute_query(pool, sql).await
/// });
/// ```
#[macro_export]
macro_rules! measure_latency {
    ($operation:expr, $body:expr) => {{
        let start = std::time::Instant::now();
        let result = $body;
        let duration = start.elapsed();
        tracing::info!("Performance: {} took {:?}", $operation, duration);
        if duration.as_millis() > 100 {
            tracing::warn!(
                "Slow operation detected: {} took {:?} (target: <100ms)",
                $operation,
                duration
            );
        }
        result
    }};
}

/// レイテンシ計測用のガード
///
/// RAIIパターンで自動的に計測開始・終了。
/// finish()を呼んだ場合はinfo、Drop時（途中破棄）はdebugでログを出力する。
/// finish()後にDropが走っても二重ログにならないよう finished フラグで制御する。
pub struct LatencyGuard {
    operation: String,
    start: Instant,
    finished: bool,
}

impl LatencyGuard {
    /// 新しいレイテンシ計測を開始
    pub fn new(operation: impl Into<String>) -> Self {
        Self {
            operation: operation.into(),
            start: Instant::now(),
            finished: false,
        }
    }

    /// 計測を終了して経過時間を返す
    pub fn finish(mut self) -> Duration {
        self.finished = true;
        let duration = self.start.elapsed();
        tracing::info!("Performance: {} took {:?}", self.operation, duration);
        if duration.as_millis() > 100 {
            tracing::warn!(
                "Slow operation detected: {} took {:?} (target: <100ms)",
                self.operation,
                duration
            );
        }
        duration
    }
}

impl Drop for LatencyGuard {
    fn drop(&mut self) {
        // finish()で既にログ出力済みの場合は二重ログを防ぐためスキップする
        if self.finished {
            return;
        }
        let duration = self.start.elapsed();
        tracing::debug!("Performance: {} took {:?}", self.operation, duration);
        if duration.as_millis() > 100 {
            tracing::warn!(
                "Slow operation detected: {} took {:?} (target: <100ms)",
                self.operation,
                duration
            );
        }
    }
}

/// レイテンシ統計情報
#[derive(Debug, Clone, Default)]
pub struct LatencyStats {
    /// 計測回数
    pub count: usize,
    /// 合計時間
    pub total: Duration,
    /// 最小時間
    pub min: Option<Duration>,
    /// 最大時間
    pub max: Option<Duration>,
}

impl LatencyStats {
    /// 新しい統計情報を作成
    pub fn new() -> Self {
        Self::default()
    }

    /// 計測結果を追加
    pub fn add(&mut self, duration: Duration) {
        self.count += 1;
        self.total += duration;

        self.min = Some(match self.min {
            Some(min) if min < duration => min,
            _ => duration,
        });

        self.max = Some(match self.max {
            Some(max) if max > duration => max,
            _ => duration,
        });
    }

    /// 平均時間を取得
    pub fn average(&self) -> Option<Duration> {
        if self.count == 0 {
            None
        } else {
            Some(self.total / self.count as u32)
        }
    }

    /// 統計情報をフォーマット
    pub fn format(&self) -> String {
        if self.count == 0 {
            return "No data".to_string();
        }

        format!(
            "count={}, avg={:?}, min={:?}, max={:?}",
            self.count,
            self.average().unwrap(),
            self.min.unwrap(),
            self.max.unwrap()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn test_latency_stats_empty() {
        let stats = LatencyStats::new();
        assert_eq!(stats.count, 0);
        assert!(stats.average().is_none());
    }

    #[test]
    fn test_latency_stats_single() {
        let mut stats = LatencyStats::new();
        let duration = Duration::from_millis(100);
        stats.add(duration);

        assert_eq!(stats.count, 1);
        assert_eq!(stats.average(), Some(duration));
        assert_eq!(stats.min, Some(duration));
        assert_eq!(stats.max, Some(duration));
    }

    #[test]
    fn test_latency_stats_multiple() {
        let mut stats = LatencyStats::new();
        stats.add(Duration::from_millis(50));
        stats.add(Duration::from_millis(100));
        stats.add(Duration::from_millis(150));

        assert_eq!(stats.count, 3);
        assert_eq!(stats.min, Some(Duration::from_millis(50)));
        assert_eq!(stats.max, Some(Duration::from_millis(150)));

        let avg = stats.average().unwrap();
        // 平均は100ms
        assert!(avg >= Duration::from_millis(99) && avg <= Duration::from_millis(101));
    }

    #[test]
    fn test_latency_guard() {
        let guard = LatencyGuard::new("test_operation");
        thread::sleep(Duration::from_millis(10));
        let duration = guard.finish();

        assert!(duration >= Duration::from_millis(10));
    }

    #[test]
    fn test_latency_stats_format() {
        let mut stats = LatencyStats::new();
        stats.add(Duration::from_millis(50));
        stats.add(Duration::from_millis(100));

        let formatted = stats.format();
        assert!(formatted.contains("count=2"));
        assert!(formatted.contains("avg="));
        assert!(formatted.contains("min="));
        assert!(formatted.contains("max="));
    }
}
