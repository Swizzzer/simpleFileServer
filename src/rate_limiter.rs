use futures::{Future, Stream};
use std::{
    pin::Pin,
    task::{Context, Poll},
    time::{Duration, Instant},
};
use tokio::time::Sleep;

/// 统一的限速器实现

#[derive(Debug)]
pub struct RateLimiter {
    /// 每秒允许的字节数
    bytes_per_second: usize,
    /// 当前时间窗口内已消耗的字节数
    bytes_consumed: usize,
    /// 当前时间窗口的开始时间
    window_start: Instant,
    /// 等待状态的 Sleep Future
    pending_sleep: Option<Pin<Box<Sleep>>>,
    /// 是否启用突发模式（允许短时间超过限制）
    burst_enabled: bool,
    /// 突发缓冲区大小（突发模式下的额外配额）
    burst_buffer: usize,
}

impl RateLimiter {
    /// - `bytes_per_second`: 每秒允许的字节数
    pub fn new(bytes_per_second: usize) -> Self {
        Self::with_burst(bytes_per_second, false, 0)
    }

    /// 支持突发的限速器
    ///
    /// - `bytes_per_second`: 每秒允许的字节数
    /// - `burst_enabled`: 是否启用突发模式
    /// - `burst_size`: 突发缓冲区大小（通常设为 bytes_per_second 的 10-20%）
    pub fn with_burst(bytes_per_second: usize, burst_enabled: bool, burst_size: usize) -> Self {
        Self {
            bytes_per_second,
            bytes_consumed: 0,
            window_start: Instant::now(),
            pending_sleep: None,
            burst_enabled,
            burst_buffer: if burst_enabled { burst_size } else { 0 },
        }
    }

    /// 检查是否可以继续发送数据
    ///
    /// # 返回值
    /// - `Poll::Ready(())`: 可以继续发送
    /// - `Poll::Pending`: 需要等待，会在适当时机唤醒
    pub fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        let now = Instant::now();

        // 检查是否需要重置时间窗口
        if self.should_reset_window(now) {
            self.reset_window(now);
            return Poll::Ready(());
        }

        // 处理正在等待的 sleep
        if let Some(ref mut sleep) = self.pending_sleep {
            match sleep.as_mut().poll(cx) {
                Poll::Ready(_) => {
                    self.pending_sleep = None;
                    // 重新检查窗口状态
                    return self.poll_ready(cx);
                }
                Poll::Pending => return Poll::Pending,
            }
        }

        // 检查是否超过速率限制
        let effective_limit = self.bytes_per_second + self.burst_buffer;
        if self.bytes_consumed >= effective_limit {
            let wait_duration = self.calculate_wait_duration(now);
            self.pending_sleep = Some(Box::pin(tokio::time::sleep(wait_duration)));
            return self.poll_ready(cx);
        }

        Poll::Ready(())
    }

    /// 消耗指定数量的字节配额
    ///
    /// # 参数
    /// - `bytes`: 要消耗的字节数
    pub fn consume(&mut self, bytes: usize) {
        self.bytes_consumed += bytes;

        // 如果启用突发模式，逐渐恢复突发缓冲区
        if self.burst_enabled && self.burst_buffer < self.bytes_per_second / 5 {
            let recovery_rate = self.bytes_per_second / 10; // 每秒恢复10%
            let elapsed = self.window_start.elapsed().as_millis() as usize;
            let recovery_amount = (recovery_rate * elapsed) / 1000;
            self.burst_buffer =
                (self.burst_buffer + recovery_amount).min(self.bytes_per_second / 5);
        }
    }

    fn should_reset_window(&self, now: Instant) -> bool {
        now.duration_since(self.window_start) >= Duration::from_secs(1)
    }

    fn reset_window(&mut self, now: Instant) {
        self.bytes_consumed = 0;
        self.window_start = now;
        self.pending_sleep = None;
    }

    fn calculate_wait_duration(&self, now: Instant) -> Duration {
        // 计算到下一个时间窗口的剩余时间
        let window_end = self.window_start + Duration::from_secs(1);
        let remaining = window_end.saturating_duration_since(now);

        let excess_ratio = self.bytes_consumed as f64 / self.bytes_per_second as f64;
        if excess_ratio > 2.0 {
            // 严重超出时，额外等待一些时间
            remaining + Duration::from_millis((excess_ratio * 100.0) as u64)
        } else {
            remaining
        }
    }
}

pub struct RateLimitedStream<S> {
    inner: S,
    rate_limiter: RateLimiter,
}

impl<S> RateLimitedStream<S> {
    /// 创建新的限速流
    pub fn new(inner: S, bytes_per_second: usize) -> Self {
        Self {
            inner,
            rate_limiter: RateLimiter::new(bytes_per_second),
        }
    }

    pub fn with_burst(inner: S, bytes_per_second: usize, burst_size: usize) -> Self {
        Self {
            inner,
            rate_limiter: RateLimiter::with_burst(bytes_per_second, true, burst_size),
        }
    }
}

impl<S> Stream for RateLimitedStream<S>
where
    S: Stream<Item = Result<bytes::Bytes, std::io::Error>> + Unpin,
{
    type Item = Result<bytes::Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Poll::Pending = self.rate_limiter.poll_ready(cx) {
            return Poll::Pending;
        }

        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                self.rate_limiter.consume(chunk.len());
                Poll::Ready(Some(Ok(chunk)))
            }
            other => other,
        }
    }
}
#[derive(Debug, Clone)]
pub struct RateLimiterFactory {
    default_rate: usize,
    burst_enabled: bool,
    burst_ratio: f64,
}

impl RateLimiterFactory {
    pub fn new(default_rate: usize) -> Self {
        Self {
            default_rate,
            burst_enabled: false,
            burst_ratio: 0.2, // 默认20%突发
        }
    }

    pub fn with_burst(mut self, burst_ratio: f64) -> Self {
        self.burst_enabled = true;
        self.burst_ratio = burst_ratio;
        self
    }

    pub fn create_limiter(&self, rate: Option<usize>) -> RateLimiter {
        let effective_rate = rate.unwrap_or(self.default_rate);
        if self.burst_enabled {
            let burst_size = (effective_rate as f64 * self.burst_ratio) as usize;
            RateLimiter::with_burst(effective_rate, true, burst_size)
        } else {
            RateLimiter::new(effective_rate)
        }
    }

    pub fn create_stream<S>(&self, stream: S, rate: Option<usize>) -> RateLimitedStream<S> {
        let effective_rate = rate.unwrap_or(self.default_rate);
        if self.burst_enabled {
            let burst_size = (effective_rate as f64 * self.burst_ratio) as usize;
            RateLimitedStream::with_burst(stream, effective_rate, burst_size)
        } else {
            RateLimitedStream::new(stream, effective_rate)
        }
    }
}
