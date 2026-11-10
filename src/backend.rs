use std::sync::{Arc, Mutex};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use tokio::process::{Child, Command};
use tracing::{debug, info, warn};
#[cfg(windows)]
use windows::Win32::{
    Foundation::{CloseHandle, HANDLE},
    System::{
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JobObjectExtendedLimitInformation, SetInformationJobObject,
        },
        Threading::{OpenProcess, PROCESS_ALL_ACCESS},
    },
};

use crate::stats::Stats;

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum BackendState {
    #[default]
    Stopped,
    Loading,
    Ready,
    Unloading,
}

pub struct SpawnSpec {
    pub exe: String,
    pub workdir: Option<String>,
    pub args: Vec<String>,
}

/// Raw Win32 handle wrapped to be Send (required by tokio/axum). Closing is
/// always done explicitly; the wrapper never drops it.
#[cfg(windows)]
#[derive(Clone, Copy)]
struct SendHandle(HANDLE);
#[cfg(windows)]
unsafe impl Send for SendHandle {}

/// One backend stdout/stderr line, without the trailing newline.
/// Console mode: sink prints it to the terminal. Silent mode: no sink,
/// nothing is captured (keeps the log file small).
pub type LineSink = std::sync::Arc<dyn Fn(String) + Send + Sync>;

pub struct Backend {
    child: Mutex<Option<Child>>,
    /// Windows job object holding the backend process tree. Kept open
    /// while the proxy runs; closing it (or the proxy dying) kills the
    /// whole tree via KILL_ON_JOB_CLOSE. On Unix the backend is the leader
    /// of its own process group (setsid at spawn) and is killed as a group.
    #[cfg(windows)]
    job: Mutex<Option<SendHandle>>,
    state: std::sync::atomic::AtomicU8,
    /// Bumped on every spawn/stop: lets a stale wait_ready() (e.g. a
    /// preempted profile switch) bail instead of polling a dead port for
    /// 300 s and then clobbering the new backend's state.
    spawn_epoch: std::sync::atomic::AtomicU64,
    last_request: std::sync::Mutex<Instant>,
    pub stats: Arc<Stats>,
    pub client: reqwest::Client,
    /// Where the backend's stdout/stderr lines go (see LineSink).
    sink: Option<LineSink>,
}

const ST_STOPPED: u8 = 0;
const ST_LOADING: u8 = 1;
const ST_READY: u8 = 2;
const ST_UNLOADING: u8 = 3;

impl Backend {
    pub fn new(stats: Arc<Stats>, client: reqwest::Client, sink: Option<LineSink>) -> Arc<Self> {
        Arc::new(Self {
            child: Mutex::new(None),
            #[cfg(windows)]
            job: Mutex::new(None),
            state: std::sync::atomic::AtomicU8::new(ST_STOPPED),
            spawn_epoch: std::sync::atomic::AtomicU64::new(0),
            last_request: Mutex::new(Instant::now()),
            stats,
            client,
            sink,
        })
    }

    pub fn state(&self) -> BackendState {
        match self.state.load(Ordering::SeqCst) {
            ST_STOPPED => BackendState::Stopped,
            ST_LOADING => BackendState::Loading,
            ST_READY => BackendState::Ready,
            _ => BackendState::Unloading,
        }
    }

    pub fn is_ready(&self) -> bool {
        self.state() == BackendState::Ready
    }

    /// PID of the backend process, if one is running.
    pub fn pid(&self) -> Option<u32> {
        self.child
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|c| c.id())
    }

    pub fn touch(&self) {
        *self.last_request.lock().unwrap() = Instant::now();
    }

    async fn set_state(&self, s: BackendState) {
        self.state.store(
            match s {
                BackendState::Stopped => ST_STOPPED,
                BackendState::Loading => ST_LOADING,
                BackendState::Ready => ST_READY,
                BackendState::Unloading => ST_UNLOADING,
            },
            Ordering::SeqCst,
        );
    }

    pub async fn spawn(&self, spec: SpawnSpec) -> Result<()> {
        if self.child.lock().unwrap().is_some() {
            bail!("backend already running");
        }
        let mut cmd = Command::new(&spec.exe);
        if let Some(wd) = &spec.workdir {
            cmd.current_dir(wd);
        }
        cmd.args(&spec.args);
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        #[cfg(unix)]
        {
            unsafe {
                cmd.pre_exec(|| {
                    // New session: the backend becomes the leader of its own
                    // process group so stop() can kill the whole tree, not
                    // just the leader (a backend may spawn children).
                    libc::setsid();
                    Ok(())
                });
            }
        }
        if self.sink.is_some() {
            // Capture and stream to the configured sink (console or file).
            cmd.stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
        } else {
            // No sink: nothing captured.
            cmd.stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
        }
        info!(exe = %spec.exe, "starting backend");
        let mut c = cmd.spawn()?;
        let stats = self.stats.clone();
        if let Some(sink) = self.sink.clone() {
            // Capture stdout/stderr and send each line to the sink.
            if let Some(out) = c.stdout.take() {
                let s = sink.clone();
                tokio::spawn(async move {
                    use tokio::io::AsyncBufReadExt;
                    let mut lines = tokio::io::BufReader::new(out).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        (s)(line);
                    }
                });
            }
            if let Some(err) = c.stderr.take() {
                tokio::spawn(async move {
                    use tokio::io::AsyncBufReadExt;
                    let mut lines = tokio::io::BufReader::new(err).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        (sink)(line);
                    }
                });
            }
        }
        self.spawn_epoch.fetch_add(1, Ordering::SeqCst);
        self.set_state(BackendState::Loading).await;
        stats.record_load_start();
        #[cfg(windows)]
        let pid = c.id().expect("child pid");
        *self.child.lock().unwrap() = Some(c);
        // Attach the backend to a job object with KILL_ON_JOB_CLOSE: no
        // matter how this process exits (tray quit, taskkill, crash),
        // Windows kills the whole process tree and frees RAM/VRAM. (On
        // Unix the equivalent is the process group, killed in stop().)
        #[cfg(windows)]
        {
        let job = unsafe {
            let j = CreateJobObjectW(None, None).expect("CreateJobObjectW");
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                j,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                core::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
            .expect("SetInformationJobObject");
            let h = OpenProcess(PROCESS_ALL_ACCESS, false, pid).expect("OpenProcess");
            AssignProcessToJobObject(j, h).expect("AssignProcessToJobObject");
            let _ = CloseHandle(h);
            j
        };
        *self.job.lock().unwrap() = Some(SendHandle(job));
        }
        Ok(())
    }

    #[cfg(windows)]
    fn close_job(&self) {
        if let Some(SendHandle(j)) = self.job.lock().unwrap().take() {
            unsafe {
                let _ = CloseHandle(j);
            }
        }
    }

    pub async fn wait_ready(&self, health_url: &str) -> Result<()> {
        let epoch = self.spawn_epoch.load(Ordering::SeqCst);
        let t0 = Instant::now();
        loop {
            // A newer spawn/stop superseded this wait (profile switched
            // mid-load): bail at once, leave the new backend's state alone.
            if self.spawn_epoch.load(Ordering::SeqCst) != epoch {
                bail!("backend spawn superseded");
            }
            match self.client.get(health_url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    self.set_state(BackendState::Ready).await;
                    let secs = t0.elapsed().as_secs_f64();
                    self.stats.record_load_done(secs);
                    info!(secs, "backend ready");
                    return Ok(());
                }
                Ok(_) => {}
                Err(e) => debug!(err = %e, "health check failed, retrying"),
            }
            // Bail early if the child died.
            let died = self
                .child
                .lock()
                .unwrap()
                .as_mut()
                .map(|c| c.try_wait())
                .unwrap_or(Ok(None))
                .ok()
                .flatten()
                .is_some();
            if died {
                self.set_state(BackendState::Stopped).await;
                self.stats.record_error();
                bail!("backend exited during load");
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
            if t0.elapsed() > Duration::from_secs(300) {
                self.set_state(BackendState::Stopped).await;
                self.stats.record_error();
                bail!("backend did not become ready within 300s");
            }
        }
    }

    pub async fn stop(&self) {
        info!("stop backend requested");
        self.spawn_epoch.fetch_add(1, Ordering::SeqCst);
        self.set_state(BackendState::Unloading).await;
        let pid = self
            .child
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|c| c.id());
        let to_kill = self.child.lock().unwrap().take();
        #[cfg(windows)]
        {
            if let Some(mut c) = to_kill {
                // Windows: the job object also takes the tree with it.
                let _ = c.kill().await;
            }
            // Closing the job handle kills the whole process tree.
            self.close_job();
        }
        #[cfg(unix)]
        {
            // Unix: SIGTERM the whole process group, give the tree up to 5 s
            // to exit, then SIGKILL whatever is left. ESRCH (already dead)
            // is harmless.
            if let Some(pid) = pid {
                let pg = -(pid as i32);
                if let Some(mut c) = to_kill {
                    unsafe {
                        libc::kill(pg, libc::SIGTERM);
                    }
                    let _ = tokio::time::timeout(Duration::from_secs(5), c.wait()).await;
                    unsafe {
                        libc::kill(pg, libc::SIGKILL);
                    }
                }
            }
        }
        self.set_state(BackendState::Stopped).await;
        info!(pid = ?pid, "backend stopped");
    }

/// Watch loop: idle-unload + dead-process detection. The idle timeout
    /// is read from the ACTIVE profile on every iteration (profiles can be
    /// switched at runtime and carry different timeouts). The unload is
    /// skipped while a request is in flight (stats.in_flight), and the
    /// proxy touches the idle timer at request END (not start), so a long
    /// generation never counts toward the idle window.
    pub async fn watch(
        self: Arc<Self>,
        idle_secs: impl Fn() -> u64 + Send + Sync + 'static,
    ) {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let died = {
                let mut child = self.child.lock().unwrap();
                let died = child
                    .as_mut()
                    .map(|c| c.try_wait())
                    .unwrap_or(Ok(None))
                    .ok() // swallow Err (would be unexpected)
                    .flatten()
                    .is_some();
                if died {
                    child.take();
                }
                died
            };
            if died {
                warn!("backend process exited unexpectedly");
                self.stats.record_error();
                self.set_state(BackendState::Stopped).await;
                #[cfg(windows)]
                self.close_job();
            }

            let idle_secs = idle_secs();
            if idle_secs > 0
                && self.state() == BackendState::Ready
                && !self.stats.in_flight()
            {
                let idle = self.last_request.lock().unwrap().elapsed();
                if idle.as_secs() >= idle_secs {
                    info!(idle_secs = idle.as_secs(), "idle timeout, unloading backend");
                    self.stop().await;
                }
            }
        }
    }
}
