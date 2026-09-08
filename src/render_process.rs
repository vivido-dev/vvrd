//! A document backend may block in native code. Only the disposable worker owns it.
use crate::{
    child_process,
    compositor::validate_page,
    geometry::WindowSize,
    mailbox,
    renderer::{PaperStyle, RenderCmd, RenderEvent},
};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    io::{self, Read, Write},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const MAX_METADATA: usize = 8 * 1024 * 1024;
const MAX_PIXELS: usize = 16_700_000 * 3;
const MAX_DOCUMENT_PAGES: usize = 100_000;
const POLL: Duration = Duration::from_millis(20);

pub fn export_page(
    path: &std::path::Path,
    requested: usize,
    viewport: WindowSize,
    options: crate::renderer::RenderOptions,
    auto_crop: bool,
    style: PaperStyle,
) -> anyhow::Result<PathBuf> {
    let worker = RenderThread::spawn(path.to_owned(), viewport, style);
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut rendered = false;
    let mut count = None;
    let mut sent_render = false;
    while !rendered || count.is_none() {
        match worker
            .events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))?
        {
            RenderEvent::Opened {
                n_pages,
                pagination_complete,
                ..
            } => {
                if pagination_complete {
                    count = Some(n_pages);
                }
                if !sent_render {
                    // Page zero triggers the background count without speculating past EOF.
                    worker.commands.send(RenderCmd::Render {
                        page: 0,
                        options: options.clone(),
                    })?;
                    sent_render = true;
                }
            }
            RenderEvent::Page { .. } => rendered = true,
            RenderEvent::Error(error) => anyhow::bail!("{error}"),
            _ => {}
        }
    }
    let count = count.unwrap_or(1);
    let page = requested.min(count.saturating_sub(1));
    let output = crate::export::next_export_path(path, page, count)?;
    worker.commands.send(RenderCmd::Export {
        page,
        output: output.clone(),
        options,
        auto_crop,
    })?;
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        match worker
            .events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))?
        {
            RenderEvent::Exported(_) => break,
            RenderEvent::Error(error) => anyhow::bail!("{error}"),
            _ => {}
        }
    }
    worker.shutdown();
    Ok(output)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Open {
    path: PathBuf,
    viewport: WindowSize,
    style: PaperStyle,
}

pub struct RenderThread {
    pub commands: mailbox::Sender<RenderCmd>,
    pub events: mailbox::Receiver<RenderEvent>,
    stopped: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    join: Option<JoinHandle<()>>,
}

impl RenderThread {
    pub fn spawn(path: PathBuf, viewport: WindowSize, style: PaperStyle) -> Self {
        let (commands, receive) = mailbox::channel();
        let (publish, events) = mailbox::channel();
        let stopped = Arc::new(AtomicBool::new(false));
        let child = Arc::new(Mutex::new(None));
        let stop = stopped.clone();
        let process = child.clone();
        let join = thread::spawn(move || {
            supervise(
                Open {
                    path,
                    viewport,
                    style,
                },
                receive,
                publish,
                stop,
                process,
            )
        });
        Self {
            commands,
            events,
            stopped,
            child,
            join: Some(join),
        }
    }
    pub fn shutdown(mut self) {
        self.stop();
    }
    fn stop(&mut self) {
        let Some(join) = self.join.take() else {
            return;
        };
        self.stopped.store(true, Ordering::Release);
        self.commands.close();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !join.is_finished() && Instant::now() < deadline {
            thread::sleep(POLL);
        }
        kill(&self.child);
        let _ = join.join();
    }
}
impl Drop for RenderThread {
    fn drop(&mut self) {
        self.stop();
    }
}

fn kill(child: &Mutex<Option<Child>>) {
    if let Some(mut process) = child
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
    {
        child_process::kill_tree(&mut process);
    }
}

fn executable() -> io::Result<PathBuf> {
    let path = std::env::current_exe()?;
    if cfg!(test) {
        let parent = path
            .parent()
            .and_then(|path| path.parent())
            .ok_or_else(|| io::Error::other("test executable path missing"))?;
        Ok(parent.join(if cfg!(windows) { "vvrd.exe" } else { "vvrd" }))
    } else {
        Ok(path)
    }
}

fn supervise(
    open: Open,
    commands: mailbox::Receiver<RenderCmd>,
    events: mailbox::Sender<RenderEvent>,
    stopped: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
) {
    let mut pending = None;
    let mut first = true;
    let mut revision_base = 0u64;
    while !stopped.load(Ordering::Acquire) {
        if !first {
            // Never restart on a timer: an explicit new request is required after failure.
            match commands.recv_timeout(POLL) {
                Ok(RenderCmd::Shutdown) | Err(flume::RecvTimeoutError::Disconnected) => break,
                Ok(command) => pending = Some(command),
                Err(flume::RecvTimeoutError::Timeout) => continue,
            }
        }
        first = false;
        let result = run_worker(
            &open,
            &commands,
            &events,
            &stopped,
            &child,
            &mut pending,
            &mut revision_base,
        );
        kill(&child);
        if stopped.load(Ordering::Acquire) {
            break;
        }
        if let Err(error) = result {
            let _ = events.send(RenderEvent::Error(format!(
                "document worker failed: {error}; retry to restart"
            )));
        }
        // Discard work queued before failure; it must not trigger an automatic crash loop.
        while commands.try_recv().is_ok() {}
        pending = None;
    }
}

fn publish(
    events: &mailbox::Sender<RenderEvent>,
    mut event: RenderEvent,
    stopped: &AtomicBool,
) -> io::Result<()> {
    // The result path is bounded too. Retry while the UI drains it, but never hold up shutdown.
    loop {
        if stopped.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "renderer stopped",
            ));
        }
        match events.send_recover(event) {
            Ok(()) => return Ok(()),
            Err((error, value)) if error.kind() == io::ErrorKind::WouldBlock => {
                event = value;
                thread::sleep(POLL);
            }
            Err((error, _)) => return Err(error),
        }
    }
}

fn run_worker(
    open: &Open,
    commands: &mailbox::Receiver<RenderCmd>,
    events: &mailbox::Sender<RenderEvent>,
    stopped: &AtomicBool,
    child_slot: &Mutex<Option<Child>>,
    pending: &mut Option<RenderCmd>,
    revision_base: &mut u64,
) -> io::Result<()> {
    let mut command = Command::new(executable()?);
    child_process::isolate(&mut command)
        .arg("--render-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn()?;
    let mut input = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("worker stdin unavailable"))?;
    let mut output = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("worker stdout unavailable"))?;
    *child_slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(child);
    let (tx, rx) = flume::bounded(1);
    let reader = thread::spawn(move || {
        loop {
            let event = read_event(&mut output);
            let failed = event.is_err();
            if tx.send(event).is_err() || failed {
                break;
            }
        }
    });
    let (writes, requests) = flume::bounded::<Vec<u8>>(2);
    let writer = thread::spawn(move || {
        while let Ok(bytes) = requests.recv() {
            if input
                .write_all(&bytes)
                .and_then(|()| input.flush())
                .is_err()
            {
                break;
            }
        }
    });
    let result = (|| {
        enqueue(&writes, open)?;
        let mut deadline = Instant::now()
            + if crate::renderer::detect_backend(&open.path)
                == crate::renderer::RenderBackend::Office
            {
                Duration::from_secs(120)
            } else {
                Duration::from_secs(30)
            };
        let base = *revision_base;
        let mut local_revision = 1;
        let mut sequence = 0u64;
        let mut busy = true;
        let mut expected_page = None;
        loop {
            if stopped.load(Ordering::Acquire) {
                let _ = enqueue(&writes, &RenderCmd::Shutdown);
                return Ok(());
            }
            if busy && Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "document operation deadline exceeded",
                ));
            }
            match rx.recv_timeout(POLL) {
                Ok(Ok(RenderEvent::Idle(id))) => {
                    if id != sequence {
                        return Err(io::Error::other("unexpected worker completion sequence"));
                    }
                    busy = false;
                }
                Ok(Ok(RenderEvent::Stopped)) => {
                    return Err(io::Error::other("worker stopped unexpectedly"));
                }
                Ok(Ok(mut event)) => {
                    match &mut event {
                        RenderEvent::Opened {
                            document_revision, ..
                        } => {
                            if *document_revision < local_revision {
                                continue;
                            }
                            local_revision = *document_revision;
                            *document_revision = base
                                .checked_add(local_revision)
                                .ok_or_else(|| io::Error::other("document revision exhausted"))?;
                            *revision_base = *document_revision;
                        }
                        RenderEvent::Page {
                            page,
                            generation,
                            document_revision,
                            ..
                        } => {
                            if expected_page != Some((*page, *generation))
                                || *document_revision != local_revision
                            {
                                continue;
                            }
                            *document_revision = base
                                .checked_add(local_revision)
                                .ok_or_else(|| io::Error::other("document revision exhausted"))?;
                        }
                        _ => {}
                    }
                    publish(events, event, stopped)?;
                }
                Ok(Err(error)) => return Err(error),
                Err(flume::RecvTimeoutError::Disconnected) => {
                    return Err(io::Error::other("worker pipe closed"));
                }
                Err(flume::RecvTimeoutError::Timeout) => {}
            }
            if !busy {
                let next = pending.take().or_else(|| commands.try_recv().ok());
                if let Some(next) = next {
                    if matches!(next, RenderCmd::Shutdown) {
                        return Ok(());
                    }
                    sequence = sequence
                        .checked_add(1)
                        .ok_or_else(|| io::Error::other("worker sequence exhausted"))?;
                    expected_page = match &next {
                        RenderCmd::Render { page, options } => Some((*page, options.generation)),
                        _ => None,
                    };
                    deadline = Instant::now()
                        + match next {
                            RenderCmd::Search(_) | RenderCmd::Export { .. } | RenderCmd::Reload => {
                                Duration::from_secs(120)
                            }
                            _ => Duration::from_secs(30),
                        };
                    enqueue(&writes, &next)?;
                    enqueue(&writes, &RenderCmd::Fence(sequence))?;
                    busy = true;
                }
            }
        }
    })();
    drop(writes);
    kill(child_slot);
    drop(rx);
    let _ = reader.join();
    let _ = writer.join();
    result
}

fn enqueue(writes: &flume::Sender<Vec<u8>>, value: &impl Serialize) -> io::Result<()> {
    let mut bytes = Vec::new();
    write_record(&mut bytes, value, &[])?;
    writes
        .try_send(bytes)
        .map_err(|_| io::Error::other("worker command pipe unavailable"))
}

fn write_record(writer: &mut impl Write, value: &impl Serialize, pixels: &[u8]) -> io::Result<()> {
    let metadata = serde_json::to_vec(value).map_err(io::Error::other)?;
    if metadata.len() > MAX_METADATA || pixels.len() > MAX_PIXELS {
        return Err(io::Error::other("worker record exceeds limits"));
    }
    writer.write_all(&(metadata.len() as u32).to_le_bytes())?;
    writer.write_all(&(pixels.len() as u64).to_le_bytes())?;
    writer.write_all(&metadata)?;
    writer.write_all(pixels)?;
    writer.flush()
}

fn read_metadata<T: DeserializeOwned>(reader: &mut impl Read) -> io::Result<(T, usize)> {
    let mut header = [0u8; 12];
    reader.read_exact(&mut header)?;
    let metadata = u32::from_le_bytes(header[..4].try_into().expect("fixed header")) as usize;
    let pixels = u64::from_le_bytes(header[4..].try_into().expect("fixed header"));
    if metadata > MAX_METADATA || pixels > MAX_PIXELS as u64 {
        return Err(io::Error::other("worker record exceeds limits"));
    }
    let mut bytes = vec![0; metadata];
    reader.read_exact(&mut bytes)?;
    Ok((
        serde_json::from_slice(&bytes).map_err(io::Error::other)?,
        pixels as usize,
    ))
}

fn read_event(reader: &mut impl Read) -> io::Result<RenderEvent> {
    let (mut event, length) = read_metadata::<RenderEvent>(reader)?;
    let valid = match &event {
        RenderEvent::Opened { n_pages, toc, .. } => {
            *n_pages <= MAX_DOCUMENT_PAGES
                && toc
                    .iter()
                    .all(|entry| entry.page < MAX_DOCUMENT_PAGES && entry.level <= 256)
        }
        RenderEvent::Page { page, links, .. } => *page < MAX_DOCUMENT_PAGES && valid_links(links),
        RenderEvent::SearchComplete(counts) => {
            counts.len() <= MAX_DOCUMENT_PAGES
                && counts
                    .iter()
                    .try_fold(0usize, |sum, count| sum.checked_add(*count))
                    .is_some()
        }
        RenderEvent::Links(links) => valid_links(links),
        _ => true,
    };
    if !valid {
        return Err(io::Error::other("worker document metadata exceeds limits"));
    }
    if let RenderEvent::Page { image, .. } = &mut event {
        let image = Arc::get_mut(image).ok_or_else(|| io::Error::other("shared decoded image"))?;
        crate::markup::mermaid::validate_raster_size(image.width, image.height)
            .map_err(io::Error::other)?;
        if image.row_stride != image.width as usize * 3
            || image.row_stride.checked_mul(image.height as usize) != Some(length)
        {
            return Err(io::Error::other("invalid worker image geometry"));
        }
        image.pixels.resize(length, 0);
        reader.read_exact(&mut image.pixels)?;
        validate_page(image).map_err(io::Error::other)?;
    } else if length != 0 {
        return Err(io::Error::other("unexpected worker pixel payload"));
    }
    Ok(event)
}

fn valid_links(links: &[crate::renderer::LinkInfo]) -> bool {
    links
        .iter()
        .all(|link| link.page.is_none_or(|page| page < MAX_DOCUMENT_PAGES))
}

type PaginationOpen = (PathBuf, crate::renderer::ReflowLayout, bool);

pub(crate) fn pagination_main() -> io::Result<()> {
    let ((path, layout, landscape), pixels) = read_metadata::<PaginationOpen>(&mut io::stdin())?;
    if pixels != 0 {
        return Err(io::Error::other("unexpected pagination pixels"));
    }
    crate::mupdf_fonts::install();
    let result =
        crate::renderer::paginate_reflowable(&path, layout, landscape).map_err(|e| e.to_string());
    write_record(&mut io::stdout(), &result, &[])
}

pub(crate) fn paginate(
    path: &std::path::Path,
    layout: crate::renderer::ReflowLayout,
    landscape: bool,
    superseded: impl Fn() -> bool,
) -> io::Result<crate::renderer::PaginationResult> {
    let mut command = Command::new(executable()?);
    // Inherit the rendering worker's process group, so parent cancellation kills both workers.
    child_process::scrub(&mut command)
        .arg("--paginate-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn()?;
    let mut input = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("missing pagination input"))?;
    let mut output = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("missing pagination output"))?;
    let (tx, rx) = flume::bounded(1);
    let reader = thread::spawn(move || {
        let result =
            read_metadata::<Result<crate::renderer::PaginationResult, String>>(&mut output)
                .and_then(|(result, pixels)| {
                    if pixels != 0 {
                        Err(io::Error::other("unexpected pagination pixels"))
                    } else {
                        result.map_err(io::Error::other)
                    }
                });
        let _ = tx.send(result);
    });
    let result = (|| {
        write_record(&mut input, &(path, layout, landscape), &[])?;
        drop(input);
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            if superseded() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "pagination superseded",
                ));
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "pagination timed out",
                ));
            }
            match rx.recv_timeout(POLL) {
                Ok(result) => return result,
                Err(flume::RecvTimeoutError::Disconnected) => {
                    return Err(io::Error::other("pagination pipe closed"));
                }
                Err(flume::RecvTimeoutError::Timeout) => {}
            }
        }
    })();
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader.join();
    result
}

pub fn worker_main() -> io::Result<()> {
    let mut input = io::stdin();
    let (open, pixels) = read_metadata::<Open>(&mut input)?;
    if pixels != 0 {
        return Err(io::Error::other("invalid worker initialization"));
    }
    let (commands, receive) = mailbox::channel();
    thread::spawn(move || {
        while let Ok((command, 0)) = read_metadata::<RenderCmd>(&mut input) {
            if commands.send(command).is_err() {
                break;
            }
        }
        commands.close();
    });
    let (events, result) = mailbox::channel();
    let output = thread::spawn(move || -> io::Result<()> {
        let mut output = io::stdout().lock();
        while let Ok(event) = result.recv() {
            let bytes = match &event {
                RenderEvent::Page { image, .. } => image.pixels.as_slice(),
                _ => &[],
            };
            write_record(&mut output, &event, bytes)?;
        }
        Ok(())
    });
    crate::mupdf_fonts::install();
    crate::renderer::run_render_thread(open.path, open.viewport, open.style, receive, events);
    output
        .join()
        .map_err(|_| io::Error::other("worker output thread panicked"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipc_rejects_inconsistent_dimensions_and_round_trips_pixels() {
        let page = crate::compositor::PageImage {
            pixels: vec![1, 2, 3],
            width: 1,
            height: 1,
            row_stride: 3,
            highlights: Vec::new(),
        };
        let mut event = RenderEvent::Page {
            page: 0,
            generation: 7,
            document_revision: 3,
            image: Arc::new(page),
            text: String::new(),
            links: Vec::new(),
        };
        let mut bytes = Vec::new();
        write_record(&mut bytes, &event, &[1, 2, 3]).unwrap();
        assert!(
            matches!(read_event(&mut bytes.as_slice()).unwrap(), RenderEvent::Page { image, .. } if image.pixels == [1, 2, 3])
        );
        if let RenderEvent::Page { image, .. } = &mut event {
            Arc::get_mut(image).unwrap().width = 2;
        }
        bytes.clear();
        write_record(&mut bytes, &event, &[1, 2, 3]).unwrap();
        assert!(read_event(&mut bytes.as_slice()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn shutdown_kills_a_stopped_worker_without_waiting_for_native_code() {
        let path = std::env::temp_dir().join(format!("vvrd-worker-stop-{}.md", std::process::id()));
        std::fs::write(&path, "# Stop test").unwrap();
        let worker = RenderThread::spawn(
            path.clone(),
            WindowSize::from_cells(4, 5, 1, 1),
            PaperStyle {
                theme: crate::markup::ThemeMode::Light,
                landscape: false,
            },
        );
        assert!(matches!(
            worker.events.recv_timeout(Duration::from_secs(10)).unwrap(),
            RenderEvent::Opened { .. }
        ));
        let pid = worker.child.lock().unwrap().as_ref().unwrap().id() as i32;
        // SAFETY: the PID belongs to this test's live, owned subprocess.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);
        let start = Instant::now();
        worker.shutdown();
        assert!(start.elapsed() < Duration::from_secs(3));
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn rejects_oversized_or_truncated_ipc_without_pixel_allocation() {
        let mut header = Vec::new();
        header.extend_from_slice(&0u32.to_le_bytes());
        header.extend_from_slice(&u64::MAX.to_le_bytes());
        assert!(read_event(&mut header.as_slice()).is_err());
        assert!(read_event(&mut [0u8; 4].as_slice()).is_err());
    }

    #[test]
    fn rejects_unbounded_document_page_counts() {
        let event = RenderEvent::Opened {
            kind: crate::renderer::DocumentKind::Fixed,
            n_pages: usize::MAX,
            toc: Vec::new(),
            metadata: Vec::new(),
            document_revision: 1,
            reloaded: false,
            pagination_complete: true,
        };
        let mut bytes = Vec::new();
        write_record(&mut bytes, &event, &[]).unwrap();
        assert!(read_event(&mut bytes.as_slice()).is_err());
    }
}
