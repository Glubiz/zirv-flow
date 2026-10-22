//! The pool client: a fixed number of render-host children, one render at a time in each, a
//! per-render deadline, and kill-and-replace for any child that overruns or dies.

use std::borrow::Cow;
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tracing::{error, warn};
use zirv_flow_templates::{
    Hooks, RenderContext, RenderRequest, RenderResponse, Rendered, RenderedWebhook,
    TemplateDocument, apply_tracking,
};

use crate::failure::RenderFailure;
use crate::frame::{FrameError, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES};
use crate::frame::{read_frame_async, write_frame_async};
use crate::host::READY;
use crate::limit;

/// How much of a child's standard error the pool keeps to explain its death.
const STDERR_TAIL_BYTES: usize = 4096;

/// The longest stderr line the parent logs; the rest of a longer line is dropped.
const STDERR_LINE_BYTES: usize = 2048;

/// How long a dead or failing child is given to exit by itself before it is killed.
const REAP_GRACE: Duration = Duration::from_secs(2);

/// How long the pool waits for a child's standard error to end after the child is gone.
const STDERR_GRACE: Duration = Duration::from_secs(1);

/// The line the Rust runtime prints before it aborts on a failed allocation.
const ALLOC_FAILED_PREFIX: &str = "memory allocation of ";
const ALLOC_FAILED_SUFFIX: &str = " bytes failed";

/// What to run as a render-host child.
#[derive(Clone, Debug)]
pub struct ChildCommand {
    program: PathBuf,
    args: Vec<OsString>,
    envs: Vec<(OsString, OsString)>,
    skip_stdout_until: Option<Vec<u8>>,
    memory_limit_bytes: Option<u64>,
}

impl ChildCommand {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            envs: Vec::new(),
            skip_stdout_until: None,
            memory_limit_bytes: None,
        }
    }

    /// The binary's hidden `render-host` subcommand, limiting its own memory to
    /// `memory_limit_bytes` (see `limit`).
    pub fn render_host(program: impl Into<PathBuf>, memory_limit_bytes: u64) -> Self {
        Self::new(program)
            .arg("render-host")
            .arg("--memory-limit-bytes")
            .arg(memory_limit_bytes.to_string())
            .memory_limit_bytes(Some(memory_limit_bytes))
    }

    /// The limit the children are asked to apply to themselves, so the pool can warn once at
    /// start if this platform cannot enforce it.
    #[must_use]
    pub fn memory_limit_bytes(mut self, limit: Option<u64>) -> Self {
        self.memory_limit_bytes = limit;
        self
    }

    #[must_use]
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    #[must_use]
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.envs.push((key.into(), value.into()));
        self
    }

    /// Discard everything the child writes to standard output up to and including `marker`,
    /// before its first frame. For a child that is a test harness printing a banner first.
    #[must_use]
    pub fn skip_stdout_until(mut self, marker: impl Into<Vec<u8>>) -> Self {
        self.skip_stdout_until = Some(marker.into());
        self
    }
}

/// The pool's size and the deadline of one render.
#[derive(Clone, Copy, Debug)]
pub struct PoolConfig {
    /// How many children, and so how many renders at once. At least 1.
    pub size: usize,
    /// How long one render may take, from sending the request to reading the response.
    pub deadline: Duration,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            size: 2,
            deadline: Duration::from_secs(5),
        }
    }
}

/// One child and its pipes.
struct Host {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    stderr_tail: Arc<Mutex<Vec<u8>>>,
    stderr_task: JoinHandle<()>,
    /// Still to be read off standard output before the first frame.
    skip_until: Option<Vec<u8>>,
}

impl Drop for Host {
    fn drop(&mut self) {
        self.stderr_task.abort();
    }
}

/// How a child ended, as far as the pool can tell.
struct Exit {
    /// `None` when the child was still running and the pool killed it.
    status: Option<ExitStatus>,
    /// Whether standard error holds the failed-allocation line.
    out_of_memory: bool,
}

impl Exit {
    fn failed(&self) -> bool {
        self.status.is_some_and(|status| !status.success())
    }

    /// The child exited by itself with status 0.
    fn clean(&self) -> bool {
        self.status.is_some_and(|status| status.success())
    }
}

/// Read the child's standard error in fixed-size chunks until it ends. The parent keeps only the
/// last [`STDERR_TAIL_BYTES`] for classification and logs each line cut to [`STDERR_LINE_BYTES`],
/// so what the child writes, however much and however it is broken into lines, costs bounded
/// memory.
async fn drain_stderr(mut stderr: impl AsyncRead + Unpin, tail: Arc<Mutex<Vec<u8>>>) {
    let mut chunk = [0u8; 4096];
    let mut line: Vec<u8> = Vec::new();
    let mut cut = false;
    loop {
        let read = match stderr.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        {
            let mut tail = tail.lock().unwrap_or_else(PoisonError::into_inner);
            tail.extend_from_slice(&chunk[..read]);
            if tail.len() > STDERR_TAIL_BYTES {
                let excess = tail.len() - STDERR_TAIL_BYTES;
                tail.drain(..excess);
            }
        }
        for &byte in &chunk[..read] {
            if byte == b'\n' {
                log_line(&line, cut);
                line.clear();
                cut = false;
            } else if line.len() < STDERR_LINE_BYTES {
                line.push(byte);
            } else {
                cut = true;
            }
        }
    }
    if !line.is_empty() {
        log_line(&line, cut);
    }
}

fn log_line(line: &[u8], cut: bool) {
    let text = String::from_utf8_lossy(line);
    let ellipsis = if cut { "..." } else { "" };
    warn!(target: "zirv_flow_render::child", "{text}{ellipsis}");
}

fn spawn_host(command: &ChildCommand) -> io::Result<Host> {
    let mut process = Command::new(&command.program);
    process
        .args(&command.args)
        .envs(command.envs.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = process.spawn()?;
    let missing = || io::Error::other("a child pipe is missing");
    let stdin = child.stdin.take().ok_or_else(missing)?;
    let stdout = child.stdout.take().ok_or_else(missing)?;
    let stderr = child.stderr.take().ok_or_else(missing)?;

    let stderr_tail = Arc::new(Mutex::new(Vec::new()));
    let stderr_task = tokio::spawn(drain_stderr(stderr, Arc::clone(&stderr_tail)));
    Ok(Host {
        child,
        stdin: Some(stdin),
        stdout: BufReader::new(stdout),
        stderr_tail,
        stderr_task,
        skip_until: command.skip_stdout_until.clone(),
    })
}

impl Host {
    /// Whether the child is still running.
    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    async fn skip_banner(&mut self) -> Result<(), FrameError> {
        let Some(marker) = self.skip_until.take() else {
            return Ok(());
        };
        let mut seen: Vec<u8> = Vec::new();
        let mut byte = [0u8; 1];
        while !seen.ends_with(&marker) {
            if seen.len() > 64 * 1024 {
                return Err(FrameError::TooLarge(seen.len()));
            }
            match self.stdout.read(&mut byte).await {
                Ok(0) => return Err(FrameError::Closed),
                Ok(_) => seen.push(byte[0]),
                Err(error) => return Err(FrameError::Io(error)),
            }
        }
        Ok(())
    }

    /// Wait for the ready frame the child writes once it has applied its limit.
    async fn await_ready(&mut self) -> Result<(), FrameError> {
        self.skip_banner().await?;
        let frame = read_frame_async(&mut self.stdout, READY.len() * 4).await?;
        if frame == READY {
            Ok(())
        } else {
            Err(FrameError::TooLarge(frame.len()))
        }
    }

    /// Send one request and read one response.
    async fn exchange(&mut self, request: &[u8]) -> Result<Vec<u8>, FrameError> {
        self.skip_banner().await?;
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| FrameError::Io(io::ErrorKind::BrokenPipe.into()))?;
        write_frame_async(stdin, request)
            .await
            .map_err(FrameError::Io)?;
        read_frame_async(&mut self.stdout, MAX_RESPONSE_BYTES).await
    }

    /// Wait for the child to be gone (killing it after the grace period) and for its standard
    /// error to end, then say how it ended.
    async fn reap(&mut self) -> Exit {
        let status = match timeout(REAP_GRACE, self.child.wait()).await {
            Ok(Ok(status)) => Some(status),
            Ok(Err(_)) => None,
            Err(_) => {
                let _ = self.child.kill().await;
                None
            }
        };
        let _ = timeout(STDERR_GRACE, &mut self.stderr_task).await;
        let tail = self
            .stderr_tail
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let text = String::from_utf8_lossy(&tail);
        let out_of_memory = text
            .find(ALLOC_FAILED_PREFIX)
            .is_some_and(|at| text[at..].contains(ALLOC_FAILED_SUFFIX));
        Exit {
            status,
            out_of_memory,
        }
    }

    /// Kill the child and reap it.
    async fn kill(mut self) {
        let _ = self.child.kill().await;
    }

    /// Close standard input, which asks the child to stop, and wait for it a moment.
    async fn stop(mut self) {
        self.stdin = None;
        if timeout(REAP_GRACE, self.child.wait()).await.is_err() {
            let _ = self.child.kill().await;
        }
    }
}

fn after_death(exit: &Exit) -> RenderFailure {
    if exit.out_of_memory {
        RenderFailure::MemoryLimit
    } else {
        RenderFailure::ChildExited
    }
}

async fn classify(mut host: Host, fault: FrameError) -> RenderFailure {
    match fault {
        FrameError::TooLarge(_) => {
            host.kill().await;
            RenderFailure::Protocol
        }
        FrameError::Truncated => {
            let exit = host.reap().await;
            if exit.failed() || exit.out_of_memory {
                after_death(&exit)
            } else {
                RenderFailure::Protocol
            }
        }
        FrameError::Closed | FrameError::Io(_) => {
            let exit = host.reap().await;
            if exit.clean() && !exit.out_of_memory {
                // It closed its output and left without a word: it broke the protocol, it did
                // not crash.
                RenderFailure::Protocol
            } else {
                after_death(&exit)
            }
        }
    }
}

/// Spawn a child and wait, within `deadline`, for its ready frame. A child that cannot start (a
/// limit the platform refuses, an inherited hard limit below the configured one) is found here
/// and reported with its classified reason, not accepted and failed on every render.
async fn start_host(command: &ChildCommand, deadline: Duration) -> Result<Host, RenderFailure> {
    let mut host = spawn_host(command).map_err(|error| {
        error!(%error, "could not start a render host");
        RenderFailure::ChildExited
    })?;
    match timeout(deadline, host.await_ready()).await {
        Err(_) => {
            host.kill().await;
            Err(RenderFailure::Deadline)
        }
        Ok(Ok(())) => Ok(host),
        Ok(Err(fault)) => Err(classify(host, fault).await),
    }
}

struct Inner {
    command: ChildCommand,
    deadline: Duration,
    idle: Mutex<Vec<Host>>,
    slots: Semaphore,
}

/// A pool of render-host children. Cloning shares the pool.
///
/// A render takes a free child, sends the request, and waits for the response within the
/// deadline. A child that overruns, dies, runs out of memory or sends something unreadable is
/// killed and replaced, and only the render it was serving fails.
#[derive(Clone)]
pub struct RenderPool {
    inner: Arc<Inner>,
}

impl fmt::Debug for RenderPool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RenderPool")
            .field("deadline", &self.inner.deadline)
            .field("free", &self.inner.slots.available_permits())
            .finish_non_exhaustive()
    }
}

impl RenderPool {
    /// Start `config.size` children and wait for each to report ready within the deadline. Fails
    /// if the command cannot be run or a child cannot start; the error names the reason.
    pub async fn start(command: ChildCommand, config: PoolConfig) -> io::Result<Self> {
        let size = config.size.max(1);
        limit::warn_unenforced(command.memory_limit_bytes);
        let mut hosts = Vec::with_capacity(size);
        for _ in 0..size {
            let host = start_host(&command, config.deadline)
                .await
                .map_err(|failure| {
                    io::Error::other(format!(
                        "a render host child did not become ready: {failure}"
                    ))
                })?;
            hosts.push(host);
        }
        Ok(Self {
            inner: Arc::new(Inner {
                command,
                deadline: config.deadline,
                idle: Mutex::new(hosts),
                slots: Semaphore::new(size),
            }),
        })
    }

    fn take_idle(&self) -> Option<Host> {
        loop {
            let host = self
                .inner
                .idle
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop();
            let mut host = host?;
            // A child that died while idle is dropped here and replaced by the caller.
            if host.is_alive() {
                return Some(host);
            }
        }
    }

    fn put_idle(&self, host: Host) {
        self.inner
            .idle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(host);
    }

    /// One exchange on a child of the pool.
    async fn call(
        &self,
        channel: &'static str,
        request: &[u8],
    ) -> Result<RenderResponse, RenderFailure> {
        let waiting = Instant::now();
        let Ok(_slot) = self.inner.slots.acquire().await else {
            return Err(RenderFailure::ChildExited);
        };
        metrics::histogram!("zirv_flow_render_wait_seconds").record(waiting.elapsed());

        let mut host = match self.take_idle() {
            Some(host) => host,
            None => {
                metrics::counter!("zirv_flow_render_restarts_total").increment(1);
                start_host(&self.inner.command, self.inner.deadline).await?
            }
        };

        let started = Instant::now();
        let outcome = timeout(self.inner.deadline, host.exchange(request)).await;
        metrics::histogram!("zirv_flow_render_duration_seconds", "channel" => channel)
            .record(started.elapsed());
        match outcome {
            Err(_) => {
                host.kill().await;
                Err(RenderFailure::Deadline)
            }
            Ok(Ok(bytes)) => match serde_json::from_slice::<RenderResponse>(&bytes) {
                Ok(response) => {
                    self.put_idle(host);
                    Ok(response)
                }
                Err(_) => {
                    host.kill().await;
                    Err(RenderFailure::Protocol)
                }
            },
            Ok(Err(fault)) => Err(classify(host, fault).await),
        }
    }

    async fn run(
        &self,
        channel: &'static str,
        request: &RenderRequest<'_>,
    ) -> Result<RenderResponse, RenderFailure> {
        let bytes = serde_json::to_vec(request).map_err(|_| RenderFailure::Protocol)?;
        if bytes.len() > MAX_REQUEST_BYTES {
            return Err(RenderFailure::RequestTooLarge);
        }
        self.call(channel, &bytes).await
    }

    /// Render an email in a child, with the signature of [`zirv_flow_templates::render_email`].
    ///
    /// The child renders without click tracking, because the caller's token function cannot cross
    /// the process boundary; the tracking step runs here on its result, so the bytes equal the
    /// in-process render's.
    pub async fn render_email(
        &self,
        doc: &TemplateDocument,
        context: &RenderContext,
        hooks: &Hooks<'_>,
    ) -> Result<Rendered, RenderFailure> {
        let request = RenderRequest::Email {
            doc: Cow::Borrowed(doc),
            context: Cow::Borrowed(context),
        };
        let result = async {
            let response = self.run("email", &request).await?;
            match response.into_email() {
                None => Err(RenderFailure::Protocol),
                Some(Err(error)) => Err(RenderFailure::Render(error)),
                Some(Ok(rendered)) => {
                    apply_tracking(rendered, context, hooks).map_err(RenderFailure::Render)
                }
            }
        }
        .await;
        record_failure(&result);
        result
    }

    /// Render a webhook payload in a child, with the signature of
    /// [`zirv_flow_templates::render_webhook`].
    pub async fn render_webhook(
        &self,
        doc: &TemplateDocument,
        context: &RenderContext,
    ) -> Result<RenderedWebhook, RenderFailure> {
        let request = RenderRequest::Webhook {
            doc: Cow::Borrowed(doc),
            context: Cow::Borrowed(context),
        };
        let result = async {
            let response = self.run("webhook", &request).await?;
            match response.into_webhook() {
                None => Err(RenderFailure::Protocol),
                Some(Err(error)) => Err(RenderFailure::Render(error)),
                Some(Ok(rendered)) => Ok(rendered),
            }
        }
        .await;
        record_failure(&result);
        result
    }

    /// Ask every idle child to stop by closing its input, and wait for them. Children serving a
    /// render are killed when the pool is dropped.
    pub async fn shutdown(&self) {
        let hosts = std::mem::take(
            &mut *self
                .inner
                .idle
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        for host in hosts {
            host.stop().await;
        }
    }

    /// The process ids of the idle children, for tests.
    #[cfg(test)]
    fn idle_ids(&self) -> Vec<u32> {
        self.inner
            .idle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter_map(|host| host.child.id())
            .collect()
    }

    /// Kill an idle child from outside the pool's bookkeeping, for tests.
    #[cfg(test)]
    fn kill_idle(&self) {
        let mut idle = self
            .inner
            .idle
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for host in idle.iter_mut() {
            let _ = host.child.start_kill();
        }
    }
}

fn record_failure<T>(result: &Result<T, RenderFailure>) {
    if let Err(failure) = result {
        metrics::counter!("zirv_flow_render_failures_total", "reason" => failure.reason())
            .increment(1);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use serde_json::json;
    use zirv_flow_templates::{
        Category, EmailContent, Tracking, WebhookContent, render_email, render_webhook,
    };

    use super::*;
    use crate::test_support::{capture_here, child, child_with};

    fn doc(html: &str) -> TemplateDocument {
        TemplateDocument::email_code(EmailContent {
            subject: "Hi {{ person.name }}".into(),
            sender: None,
            lang: None,
            category: Category::Transactional,
            html: html.into(),
            text: "Hi".into(),
        })
    }

    fn context() -> RenderContext {
        let mut context = RenderContext::default();
        context.person.insert("name".into(), json!("Ann"));
        context
    }

    fn config(size: usize, deadline_ms: u64) -> PoolConfig {
        PoolConfig {
            size,
            deadline: Duration::from_millis(deadline_ms),
        }
    }

    async fn ok_render(pool: &RenderPool) {
        let rendered = pool
            .render_email(
                &doc("<p>{{ person.name }}</p>"),
                &context(),
                &Hooks::untracked(),
            )
            .await
            .unwrap();
        assert_eq!(rendered.html, "<p>Ann</p>");
    }

    fn tok(index: usize, _url: &str) -> String {
        format!("tok{index}")
    }

    fn fixtures() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../zirv-flow-templates/tests/fixtures")
    }

    // ----- results equal the in-process renderer -----------------------------------------

    #[tokio::test]
    async fn the_golden_corpus_renders_to_the_same_bytes_as_in_process() {
        let pool = RenderPool::start(child("serve"), config(1, 20_000))
            .await
            .unwrap();
        let hooks = Hooks {
            tracking: Tracking::Rewrite {
                base_url: "https://t.example/c",
                token: &tok,
            },
        };
        for name in ["email_marketing", "email_transactional"] {
            let read = |suffix: &str| {
                fs::read_to_string(fixtures().join(format!("{name}.{suffix}"))).unwrap()
            };
            let doc: TemplateDocument = serde_json::from_str(&read("doc.json")).unwrap();
            let context: RenderContext = serde_json::from_str(&read("context.json")).unwrap();
            let direct = render_email(&doc, &context, &hooks).unwrap();
            let pooled = pool.render_email(&doc, &context, &hooks).await.unwrap();
            assert_eq!(pooled, direct, "{name}");
            let inert = Hooks {
                tracking: Tracking::Inert {
                    base_url: "https://t.example/c",
                },
            };
            assert_eq!(
                pool.render_email(&doc, &context, &inert).await.unwrap(),
                render_email(&doc, &context, &inert).unwrap(),
                "{name} inert"
            );
        }
        let read = |suffix: &str| {
            fs::read_to_string(fixtures().join(format!("webhook_order.{suffix}"))).unwrap()
        };
        let doc: TemplateDocument = serde_json::from_str(&read("doc.json")).unwrap();
        let context: RenderContext = serde_json::from_str(&read("context.json")).unwrap();
        assert_eq!(
            pool.render_webhook(&doc, &context).await.unwrap(),
            render_webhook(&doc, &context).unwrap()
        );
    }

    #[tokio::test]
    async fn the_renderers_own_errors_come_back_typed_and_leave_the_child_alone() {
        let pool = RenderPool::start(child("serve"), config(1, 20_000))
            .await
            .unwrap();
        let before = pool.idle_ids();
        let failure = pool
            .render_email(&doc("{{ nope.nothing }}"), &context(), &Hooks::untracked())
            .await
            .unwrap_err();
        assert_eq!(
            failure,
            RenderFailure::Render(zirv_flow_templates::RenderError::Undefined { field: "html" })
        );
        let webhook = TemplateDocument::webhook_code(WebhookContent {
            headers: Default::default(),
            body: "not json".into(),
        });
        assert!(matches!(
            pool.render_webhook(&webhook, &context()).await,
            Err(RenderFailure::Render(_))
        ));
        assert_eq!(pool.idle_ids(), before, "the same child served all three");
    }

    #[tokio::test]
    async fn a_pooled_future_can_cross_threads() {
        fn assert_send<T: Send>(_: &T) {}
        let pool = RenderPool::start(child("serve"), config(1, 20_000))
            .await
            .unwrap();
        let (doc, context, hooks) = (doc("x"), context(), Hooks::untracked());
        let future = pool.render_email(&doc, &context, &hooks);
        assert_send(&future);
        drop(future);
    }

    // ----- deadline, death and recovery ----------------------------------------------------

    #[tokio::test]
    async fn a_deadline_overrun_fails_that_render_and_a_respawned_child_serves_the_next() {
        let marker = std::env::temp_dir().join(format!("zirv-render-hang-{}", std::process::id()));
        let _ = fs::remove_file(&marker);
        let command = child_with(
            "hang_first",
            [("ZIRV_RENDER_TEST_MARKER", marker.as_os_str())],
        );
        let pool = RenderPool::start(command, config(1, 500)).await.unwrap();

        let failure = pool
            .render_email(&doc("x"), &context(), &Hooks::untracked())
            .await
            .unwrap_err();
        assert_eq!(failure, RenderFailure::Deadline);
        ok_render(&pool).await;
        let _ = fs::remove_file(&marker);
    }

    #[tokio::test]
    async fn a_child_that_dies_mid_render_fails_only_that_render_and_the_pool_recovers() {
        let marker = std::env::temp_dir().join(format!("zirv-render-die-{}", std::process::id()));
        let _ = fs::remove_file(&marker);
        let command = child_with(
            "die_first",
            [("ZIRV_RENDER_TEST_MARKER", marker.as_os_str())],
        );
        let pool = RenderPool::start(command, config(1, 20_000)).await.unwrap();
        assert_eq!(
            pool.render_email(&doc("x"), &context(), &Hooks::untracked())
                .await
                .unwrap_err(),
            RenderFailure::ChildExited
        );
        assert!(pool.idle_ids().is_empty(), "the dead child is not kept");
        ok_render(&pool).await;
        ok_render(&pool).await;
        let _ = fs::remove_file(&marker);
    }

    #[tokio::test]
    async fn a_child_killed_from_outside_while_idle_costs_nobody_a_render() {
        let pool = RenderPool::start(child("serve"), config(2, 20_000))
            .await
            .unwrap();
        let before = pool.idle_ids();
        pool.kill_idle();
        // Give the kill a moment to land before the pool looks at the children.
        tokio::time::sleep(Duration::from_millis(300)).await;
        ok_render(&pool).await;
        ok_render(&pool).await;
        let after = pool.idle_ids();
        assert!(
            before.iter().all(|id| !after.contains(id)),
            "{before:?} {after:?}"
        );
    }

    #[tokio::test]
    async fn an_allocation_abort_is_a_memory_limit_failure_everywhere() {
        let pool = RenderPool::start(child("fake_oom"), config(1, 20_000))
            .await
            .unwrap();
        let failure = pool
            .render_email(&doc("x"), &context(), &Hooks::untracked())
            .await
            .unwrap_err();
        assert_eq!(failure, RenderFailure::MemoryLimit);
    }

    #[tokio::test]
    async fn a_template_that_doubles_a_string_hits_the_os_limit_and_the_next_render_succeeds() {
        if !cfg!(target_os = "linux") {
            eprintln!("skipped: the OS memory limit is applied on Linux only");
            return;
        }
        let command = child_with("serve", [("ZIRV_RENDER_TEST_LIMIT_BYTES", "536870912")]);
        let pool = RenderPool::start(command, config(1, 60_000)).await.unwrap();
        let doubling = doc("{% set ns = namespace(s='xxxxxxxx') %}\
             {% for i in range(60) %}{% set ns.s = ns.s ~ ns.s %}{% endfor %}{{ ns.s | length }}");
        let failure = pool
            .render_email(&doubling, &context(), &Hooks::untracked())
            .await
            .unwrap_err();
        // Fuel or the recursion of the sandbox may stop the doubling first on some versions of
        // the renderer; the test pins the intended outcome, the child's memory limit.
        assert_eq!(failure, RenderFailure::MemoryLimit);
        ok_render(&pool).await;
    }

    // ----- framing ------------------------------------------------------------------------

    #[tokio::test]
    async fn oversized_truncated_and_malformed_responses_are_protocol_failures() {
        for mode in ["oversized", "truncated", "malformed", "wrong_kind"] {
            let pool = RenderPool::start(child(mode), config(1, 20_000))
                .await
                .unwrap();
            let failure = pool
                .render_email(&doc("x"), &context(), &Hooks::untracked())
                .await
                .unwrap_err();
            assert_eq!(failure, RenderFailure::Protocol, "{mode}");
        }
    }

    #[tokio::test]
    async fn a_request_over_the_bound_is_not_sent() {
        let pool = RenderPool::start(child("serve"), config(1, 20_000))
            .await
            .unwrap();
        let mut big = context();
        big.person
            .insert("blob".into(), json!("x".repeat(MAX_REQUEST_BYTES + 1)));
        assert_eq!(
            pool.render_email(&doc("x"), &big, &Hooks::untracked())
                .await
                .unwrap_err(),
            RenderFailure::RequestTooLarge
        );
        ok_render(&pool).await;
    }

    // ----- the pool ------------------------------------------------------------------------

    #[tokio::test]
    async fn renders_beyond_the_pool_size_wait_for_a_free_child() {
        let pool = RenderPool::start(child("serve"), config(1, 20_000))
            .await
            .unwrap();
        let renders = (0..6).map(|_| {
            let pool = pool.clone();
            tokio::spawn(async move {
                pool.render_email(
                    &doc("<p>{{ person.name }}</p>"),
                    &context(),
                    &Hooks::untracked(),
                )
                .await
            })
        });
        for handle in renders.collect::<Vec<_>>() {
            assert_eq!(handle.await.unwrap().unwrap().html, "<p>Ann</p>");
        }
        assert_eq!(pool.idle_ids().len(), 1);
    }

    #[tokio::test]
    async fn a_command_that_cannot_run_fails_the_start() {
        let missing = ChildCommand::new("/definitely/not/a/program");
        assert!(RenderPool::start(missing, config(1, 1000)).await.is_err());
    }

    #[tokio::test]
    async fn the_child_exits_when_its_input_closes() {
        let command = child("serve");
        let mut host = spawn_host(&command).unwrap();
        host.stdin = None;
        let status = timeout(Duration::from_secs(20), host.child.wait())
            .await
            .expect("the child stops")
            .unwrap();
        assert!(status.success(), "{status:?}");
    }

    #[tokio::test]
    async fn shutdown_stops_the_idle_children() {
        let pool = RenderPool::start(child("serve"), config(2, 20_000))
            .await
            .unwrap();
        ok_render(&pool).await;
        pool.shutdown().await;
        assert!(pool.idle_ids().is_empty());
    }

    // ----- review fixes --------------------------------------------------------------------

    #[tokio::test]
    async fn a_child_that_cannot_start_fails_the_pool_start_with_its_reason() {
        let error = RenderPool::start(child("exit_at_start"), config(2, 20_000))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("did not become ready"), "{error}");
        assert!(error.contains("child_exited"), "{error}");
    }

    #[tokio::test]
    async fn a_respawned_child_that_cannot_start_fails_the_render_that_asked_for_it() {
        let marker = std::env::temp_dir().join(format!("zirv-render-once-{}", std::process::id()));
        let _ = fs::remove_file(&marker);
        let command = child_with(
            "ready_once",
            [("ZIRV_RENDER_TEST_MARKER", marker.as_os_str())],
        );
        let pool = RenderPool::start(command, config(1, 20_000)).await.unwrap();
        ok_render(&pool).await;

        pool.kill_idle();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let failure = pool
            .render_email(&doc("x"), &context(), &Hooks::untracked())
            .await
            .unwrap_err();
        assert_eq!(failure, RenderFailure::ChildExited);
        let _ = fs::remove_file(&marker);
    }

    #[tokio::test]
    async fn the_unenforced_limit_is_warned_once_per_pool_not_per_child_or_respawn() {
        if cfg!(target_os = "linux") {
            eprintln!("skipped: Linux enforces the limit, so there is nothing to warn about");
            return;
        }
        let (_guard, logs) = capture_here();
        let command = child("serve").memory_limit_bytes(Some(512 * 1024 * 1024));
        let pool = RenderPool::start(command, config(2, 20_000)).await.unwrap();
        pool.kill_idle();
        tokio::time::sleep(Duration::from_millis(300)).await;
        ok_render(&pool).await;
        ok_render(&pool).await;
        let logs = logs();
        assert_eq!(logs.matches("no OS memory limit").count(), 1, "{logs}");
    }

    #[tokio::test]
    async fn a_child_flooding_stderr_without_newlines_costs_bounded_memory() {
        let (_guard, logs) = capture_here();
        let pool = RenderPool::start(child("stderr_flood"), config(1, 20_000))
            .await
            .unwrap();
        let failure = pool
            .render_email(&doc("x"), &context(), &Hooks::untracked())
            .await
            .unwrap_err();
        // The 2 MiB of one unbroken line is cut, and the allocation line after it still counts.
        assert_eq!(failure, RenderFailure::MemoryLimit);
        let logs = logs();
        let longest = logs.lines().map(str::len).max().unwrap_or(0);
        assert!(longest < 8 * 1024, "a logged line was {longest} bytes");
    }

    #[tokio::test]
    async fn a_render_dropped_midway_kills_its_child_and_the_pool_serves_the_next() {
        let dir = std::env::temp_dir();
        let marker = dir.join(format!("zirv-render-slow-{}", std::process::id()));
        let beat = dir.join(format!("zirv-render-beat-{}", std::process::id()));
        let _ = fs::remove_file(&marker);
        let _ = fs::remove_file(&beat);
        let command = child_with(
            "slow_first",
            [
                ("ZIRV_RENDER_TEST_MARKER", marker.as_os_str()),
                ("ZIRV_RENDER_TEST_HEARTBEAT", beat.as_os_str()),
            ],
        );
        let pool = RenderPool::start(command, config(1, 60_000)).await.unwrap();

        let dropped = timeout(
            Duration::from_millis(600),
            pool.render_email(&doc("x"), &context(), &Hooks::untracked()),
        )
        .await;
        assert!(dropped.is_err(), "the render should still be running");
        assert!(pool.idle_ids().is_empty(), "the busy child is not idle");

        // The heartbeat stops growing once the child is gone.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let first = fs::metadata(&beat).unwrap().len();
        assert!(first > 0, "the child was working");
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            fs::metadata(&beat).unwrap().len(),
            first,
            "the child is still running"
        );

        ok_render(&pool).await;
        let _ = fs::remove_file(&marker);
        let _ = fs::remove_file(&beat);
    }

    #[tokio::test]
    async fn a_child_that_closes_its_output_and_exits_cleanly_is_a_protocol_failure() {
        let pool = RenderPool::start(child("exit_clean"), config(1, 20_000))
            .await
            .unwrap();
        assert_eq!(
            pool.render_email(&doc("x"), &context(), &Hooks::untracked())
                .await
                .unwrap_err(),
            RenderFailure::Protocol
        );
    }
}
