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

#[derive(Clone)]
struct FileChunk {
    data: Arc<Bytes>,
    created_at: Instant,
}

struct SlidingWindow {
    chunks: Arc<DashMap<u64, FileChunk>>,
    window_start: AtomicU64,
    current_chunk: AtomicU64,
    last_access: AtomicU64,
}

impl SlidingWindow {
    fn new() -> Self {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        Self {
            chunks: Arc::new(DashMap::new()),
            window_start: AtomicU64::new(0),
            current_chunk: AtomicU64::new(0),
            last_access: AtomicU64::new(now),
        }
    }

    fn get_chunk(&self, chunk_index: u64) -> Option<Bytes> {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        self.last_access.store(now, Ordering::Relaxed);

        self.chunks
            .get(&chunk_index)
            .map(|entry| (*entry.data).clone())
    }

    fn add_chunk(&self, chunk_index: u64, data: Bytes) {
        let chunk = FileChunk {
            data: Arc::new(data),
            created_at: Instant::now(),
        };

        self.chunks.insert(chunk_index, chunk);
        self.current_chunk.store(chunk_index, Ordering::Relaxed);

        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        self.last_access.store(now, Ordering::Relaxed);

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

    fn find_oldest_chunk(&self) -> Option<u64> {
        self.chunks
            .iter()
            .min_by_key(|entry| entry.created_at)
            .map(|entry| *entry.key())
    }

    fn is_expired(&self) -> bool {
        let last_access = self.last_access.load(Ordering::Relaxed);
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        now - last_access > SESSION_TIMEOUT.as_secs()
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
        let session = Arc::new(SlidingWindow::new());
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
        // Question: 需要加锁吗？
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

        let session = manager.get_or_create_session(file_path.clone()).await?;

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
            return Ok(Some(data));
        }

        debug!("Cache miss for chunk {} of {:?}", chunk_index, file_path);
        let data = manager
            .read_chunk(&file_path, chunk_index, buffer_size)
            .await?;

        session.add_chunk(chunk_index, data.clone());

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
                        info!("Finished streaming file: {:?}", self.file_path);
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
                        info!("Finished streaming file: {:?}", self.file_path);
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
