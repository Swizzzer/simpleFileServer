use std::{
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::{Duration, Instant, SystemTime},
};

use bytes::Bytes;
use dashmap::DashMap;
use futures::Stream;
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt, SeekFrom},
    sync::{Mutex, RwLock},
    time::{interval, MissedTickBehavior},
};
use tracing::{debug, error, info};

use crate::rate_limiter::RateLimiter;

const WINDOW_SIZE: usize = 8;
const CLEANUP_INTERVAL: Duration = Duration::from_secs(30);
const SESSION_TIMEOUT: Duration = Duration::from_secs(300);
const PREFETCH_SIZE: usize = 8; // 预读块数
const MAX_PREFETCH_DISTANCE: u64 = 16; // 最大预读距离

#[derive(Clone)]
struct FileChunk {
    data: Arc<Bytes>,
    created_at: Instant,
    is_prefetched: bool, // 是否为预读块
}

impl FileChunk {
    fn new(data: Bytes, is_prefetched: bool) -> Self {
        Self {
            data: Arc::new(data),
            created_at: Instant::now(),
            is_prefetched,
        }
    }
}

struct SlidingWindow {
    chunks: Arc<DashMap<u64, FileChunk>>,
    window_start: AtomicU64,
    current_chunk: AtomicU64,
    buffer_size: usize,
    last_access: AtomicU64,
    prefetch_tracker: Arc<DashMap<u64, Instant>>,
    access_pattern: Arc<Mutex<Vec<u64>>>,
}

impl SlidingWindow {
    fn new(buffer_size: usize) -> Self {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        Self {
            chunks: Arc::new(DashMap::new()),
            window_start: AtomicU64::new(0),
            current_chunk: AtomicU64::new(0),
            buffer_size,
            last_access: AtomicU64::new(now),
            prefetch_tracker: Arc::new(DashMap::new()),
            access_pattern: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn get_chunk(&self, chunk_index: u64) -> Option<Bytes> {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        self.last_access.store(now, Ordering::Relaxed);

        if let Ok(mut pattern) = self.access_pattern.try_lock() {
            pattern.push(chunk_index);
            if pattern.len() > 20 {
                pattern.remove(0);
            }
        }

        self.chunks
            .get(&chunk_index)
            .map(|entry| (*entry.data).clone())
    }

    fn add_chunk(&self, chunk_index: u64, data: Bytes) {
        self.add_chunk_internal(chunk_index, data, false);
    }

    fn add_chunk_internal(&self, chunk_index: u64, data: Bytes, is_prefetched: bool) {
        let chunk = FileChunk::new(data, is_prefetched);
        self.chunks.insert(chunk_index, chunk);
        self.current_chunk.store(chunk_index, Ordering::Relaxed);

        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        self.last_access.store(now, Ordering::Relaxed);

        /// TODO: 更好的缓存清理策略
        /// 当下载速率足够高时，两个用户的请求要间隔非常接近才能利用上滑动窗口
        /// 间隔越大，用到的缓存越老，越快被清理
        /// 我，真是个笨蛋.jpg
        if self.chunks.len() > WINDOW_SIZE {
            if let Some(oldest_index) = self.find_oldest_chunk() {
                self.chunks.remove(&oldest_index);
                let current_window_start = self.window_start.load(Ordering::Relaxed);
                if oldest_index == current_window_start {
                    self.window_start
                        .store(current_window_start + 1, Ordering::Relaxed);
                }
            }
        }
    }

    fn trigger_prefetch(
        &self,
        current_chunk: u64,
        manager: Arc<FileSessionManager>,
        file_path: PathBuf,
    ) {
        if !self.should_prefetch(current_chunk) {
            return;
        }

        let prefetch_start = current_chunk + 1;
        let prefetch_chunks = self.calculate_prefetch_chunks();

        let chunks_ref = self.chunks.clone();
        let prefetch_tracker = self.prefetch_tracker.clone();
        let buffer_size = self.buffer_size;

        tokio::spawn(async move {
            for i in 0..prefetch_chunks {
                let chunk_idx = prefetch_start + i as u64;

                if chunks_ref.contains_key(&chunk_idx) {
                    continue;
                }

                if prefetch_tracker.contains_key(&chunk_idx) {
                    continue;
                }

                prefetch_tracker.insert(chunk_idx, Instant::now());
                if let Ok(data) = manager.read_chunk(&file_path, chunk_idx, buffer_size).await {
                    let chunk = FileChunk::new(data, true); // 标记为预读块
                    chunks_ref.insert(chunk_idx, chunk);
                    debug!("Prefetched chunk {} for {:?}", chunk_idx, file_path);
                }
                prefetch_tracker.remove(&chunk_idx);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
    }

    fn should_prefetch(&self, current_chunk: u64) -> bool {
        if let Ok(pattern) = self.access_pattern.try_lock() {
            if pattern.len() < 3 {
                return true;
            }

            let recent_chunks = &pattern[pattern.len().saturating_sub(3)..];
            let is_sequential = recent_chunks
                .windows(2)
                .all(|w| w[1] == w[0] + 1 || w[1] == w[0]);

            if is_sequential {
                return true;
            }

            let chunk_frequency = pattern
                .iter()
                .filter(|&&chunk| {
                    chunk >= current_chunk.saturating_sub(5) && chunk <= current_chunk + 5
                })
                .count();

            return chunk_frequency >= 2;
        }

        true // 默认预读
    }

    fn calculate_prefetch_chunks(&self) -> usize {
        let cache_usage = self.chunks.len() as f64 / WINDOW_SIZE as f64;

        if cache_usage > 0.8 {
            std::cmp::max(1, PREFETCH_SIZE / 2)
        } else if cache_usage < 0.3 {
            std::cmp::min(PREFETCH_SIZE * 2, MAX_PREFETCH_DISTANCE as usize)
        } else {
            PREFETCH_SIZE
        }
    }

    fn cleanup_prefetch_tracker(&self) {
        let cutoff = Instant::now() - Duration::from_secs(30);
        self.prefetch_tracker
            .retain(|_, &mut timestamp| timestamp > cutoff);
    }

    fn find_oldest_chunk(&self) -> Option<u64> {
        let mut oldest_prefetch = None;
        let mut oldest_normal = None;
        let mut oldest_prefetch_time = Instant::now();
        let mut oldest_normal_time = Instant::now();

        for entry in self.chunks.iter() {
            let chunk_id = *entry.key();
            let chunk = entry.value();

            if chunk.is_prefetched {
                if oldest_prefetch.is_none() || chunk.created_at < oldest_prefetch_time {
                    oldest_prefetch = Some(chunk_id);
                    oldest_prefetch_time = chunk.created_at;
                }
            } else {
                if oldest_normal.is_none() || chunk.created_at < oldest_normal_time {
                    oldest_normal = Some(chunk_id);
                    oldest_normal_time = chunk.created_at;
                }
            }
        }

        oldest_prefetch.or(oldest_normal)
    }

    fn is_expired(&self) -> bool {
        let last_access = self.last_access.load(Ordering::Relaxed);
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        now - last_access > SESSION_TIMEOUT.as_secs()
    }

    fn get_prefetch_stats(&self) -> (usize, usize, f64) {
        let total_chunks = self.chunks.len();
        let prefetch_chunks = self
            .chunks
            .iter()
            .filter(|entry| entry.value().is_prefetched)
            .count();

        let hit_rate = if total_chunks > 0 {
            (total_chunks - prefetch_chunks) as f64 / total_chunks as f64 * 100.0
        } else {
            0.0
        };

        (total_chunks, prefetch_chunks, hit_rate)
    }
}

struct FileSessionManager {
    sessions: DashMap<PathBuf, Arc<SlidingWindow>>,
    readers: DashMap<PathBuf, Arc<Mutex<File>>>,
    file_metadata: DashMap<PathBuf, (u64, SystemTime)>,
}

impl FileSessionManager {
    fn new() -> Self {
        Self {
            sessions: DashMap::new(),
            readers: DashMap::new(),
            file_metadata: DashMap::new(),
        }
    }

    async fn get_or_create_session(
        &self,
        file_path: PathBuf,
        buffer_size: usize,
    ) -> Result<Arc<SlidingWindow>, std::io::Error> {
        if let Ok(metadata) = tokio::fs::metadata(&file_path).await {
            let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);

            if let Some(entry) = self.file_metadata.get(&file_path) {
                let (_, cached_modified) = *entry.value();
                if modified != cached_modified {
                    self.sessions.remove(&file_path);
                    self.readers.remove(&file_path);
                    self.file_metadata.remove(&file_path);
                }
            }

            self.file_metadata
                .insert(file_path.clone(), (metadata.len(), modified));
        }

        if let Some(session) = self.sessions.get(&file_path) {
            return Ok(session.clone());
        }

        let file = File::open(&file_path).await?;
        let session = Arc::new(SlidingWindow::new(buffer_size));
        let reader = Arc::new(Mutex::new(file));

        self.sessions.insert(file_path.clone(), session.clone());
        self.readers.insert(file_path, reader);

        Ok(session)
    }

    fn cleanup_expired_sessions(&self) {
        let mut expired_files = Vec::new();

        for entry in self.sessions.iter() {
            if entry.value().is_expired() {
                expired_files.push(entry.key().clone());
            }
        }

        for file_path in expired_files {
            debug!("Cleaning up expired session for: {:?}", file_path);
            self.sessions.remove(&file_path);
            self.readers.remove(&file_path);
            self.file_metadata.remove(&file_path);
        }

        for entry in self.sessions.iter() {
            entry.value().cleanup_prefetch_tracker();
        }
    }

    async fn read_chunk(
        &self,
        file_path: &PathBuf,
        chunk_index: u64,
        buffer_size: usize,
    ) -> Result<Bytes, std::io::Error> {
        let reader = self.readers.get(file_path).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "File reader not found")
        })?;

        let mut file = reader.lock().await;
        let offset = chunk_index * buffer_size as u64;

        file.seek(SeekFrom::Start(offset)).await?;
        let mut buffer = vec![0u8; buffer_size];
        let bytes_read = file.read(&mut buffer).await?;

        if bytes_read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "End of file reached",
            ));
        }

        buffer.truncate(bytes_read);
        Ok(Bytes::from(buffer))
    }
}

static SESSION_MANAGER: RwLock<Option<Arc<FileSessionManager>>> = RwLock::const_new(None);

pub async fn init_session_manager() {
    let manager = Arc::new(FileSessionManager::new());

    let cleanup_manager = manager.clone();
    tokio::spawn(async move {
        let mut interval = interval(CLEANUP_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            interval.tick().await;
            cleanup_manager.cleanup_expired_sessions();
        }
    });

    let mut global_manager = SESSION_MANAGER.write().await;
    *global_manager = Some(manager);
}

async fn get_session_manager() -> Option<Arc<FileSessionManager>> {
    SESSION_MANAGER.read().await.clone()
}

enum StreamState {
    Ready,
    Reading(Pin<Box<dyn Future<Output = Result<Option<Bytes>, std::io::Error>> + Send>>),
}

pub struct SlidingWindowFileStream {
    file_path: PathBuf,
    buffer_size: usize,
    current_chunk: u64,
    file_size: u64,
    session: Arc<SlidingWindow>,
    manager: Arc<FileSessionManager>,
    rate_limiter: Option<RateLimiter>,
    state: StreamState,
}

impl SlidingWindowFileStream {
    pub async fn new(
        file_path: PathBuf,
        buffer_size: usize,
        rate_limiter: Option<RateLimiter>,
    ) -> Result<Self, std::io::Error> {
        let metadata = tokio::fs::metadata(&file_path).await?;
        let file_size = metadata.len();

        let manager = get_session_manager().await.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::Other, "Session manager not initialized")
        })?;

        let session = manager
            .get_or_create_session(file_path.clone(), buffer_size)
            .await?;

        Ok(Self {
            file_path,
            buffer_size,
            current_chunk: 0,
            file_size,
            session,
            manager,
            rate_limiter,
            state: StreamState::Ready,
        })
    }

    async fn read_next_chunk(
        session: Arc<SlidingWindow>,
        manager: Arc<FileSessionManager>,
        file_path: PathBuf,
        chunk_index: u64,
        buffer_size: usize,
        file_size: u64,
    ) -> Result<Option<Bytes>, std::io::Error> {
        let chunk_offset = chunk_index * buffer_size as u64;
        if chunk_offset >= file_size {
            return Ok(None);
        }

        if let Some(data) = session.get_chunk(chunk_index) {
            debug!("Cache hit for chunk {} of {:?}", chunk_index, file_path);

            session.trigger_prefetch(chunk_index, manager.clone(), file_path.clone());

            return Ok(Some(data));
        }

        debug!("Cache miss for chunk {} of {:?}", chunk_index, file_path);
        let data = manager
            .read_chunk(&file_path, chunk_index, buffer_size)
            .await?;

        session.add_chunk(chunk_index, data.clone());
        session.trigger_prefetch(chunk_index, manager, file_path);

        Ok(Some(data))
    }
}

impl Stream for SlidingWindowFileStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            if let Some(ref mut rate_limiter) = self.rate_limiter {
                if let Poll::Pending = rate_limiter.poll_ready(cx) {
                    return Poll::Pending;
                }
            }

            match &mut self.state {
                StreamState::Ready => {
                    let chunk_offset = self.current_chunk * self.buffer_size as u64;
                    if chunk_offset >= self.file_size {
                        // 输出最终预读统计
                        let (total, prefetched, hit_rate) = self.session.get_prefetch_stats();
                        info!("Finished streaming file: {:?}, Cache stats: total={}, prefetched={}, hit_rate={:.1}%", 
                              self.file_path, total, prefetched, hit_rate);
                        return Poll::Ready(None);
                    }

                    let session = self.session.clone();
                    let manager = self.manager.clone();
                    let file_path = self.file_path.clone();
                    let chunk_index = self.current_chunk;
                    let buffer_size = self.buffer_size;
                    let file_size = self.file_size;

                    let future = Box::pin(Self::read_next_chunk(
                        session,
                        manager,
                        file_path,
                        chunk_index,
                        buffer_size,
                        file_size,
                    ));

                    self.state = StreamState::Reading(future);
                }
                StreamState::Reading(future) => match future.as_mut().poll(cx) {
                    Poll::Ready(Ok(Some(data))) => {
                        let data_len = data.len();
                        self.current_chunk += 1;
                        self.state = StreamState::Ready;

                        if let Some(ref mut rate_limiter) = self.rate_limiter {
                            rate_limiter.consume(data_len);
                        }

                        return Poll::Ready(Some(Ok(data)));
                    }
                    Poll::Ready(Ok(None)) => {
                        let (total, prefetched, hit_rate) = self.session.get_prefetch_stats();
                        info!("Finished streaming file: {:?}, Final cache stats: total={}, prefetched={}, hit_rate={:.1}%", 
                                  self.file_path, total, prefetched, hit_rate);
                        self.state = StreamState::Ready;
                        return Poll::Ready(None);
                    }
                    Poll::Ready(Err(e)) => {
                        error!(
                            "Error reading chunk {} from {:?}: {}",
                            self.current_chunk, self.file_path, e
                        );
                        self.state = StreamState::Ready;
                        return Poll::Ready(Some(Err(e)));
                    }
                    Poll::Pending => {
                        return Poll::Pending;
                    }
                },
            }
        }
    }
}
