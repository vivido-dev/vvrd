mod app;
mod compositor;
mod error;
mod export;
mod geometry;
mod presenter;
mod renderer;
mod state;
mod terminal;
mod vivid_thread;

use std::{
    io::IsTerminal as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context as _, bail};
use app::{App, InputMode, ScrollAction, StatusMsg};
use clap::Parser;
use compositor::{PageImage, ViewTransform};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use geometry::WindowSize;
use renderer::{RenderCmd, RenderEvent, RenderOptions, RenderThread};
use vivid_protocol::messages;
use vivid_sdk::{ProducerConfig, ProducerSession};
use vivid_thread::{PresentCmd, PresentEvent, VividThread};

const RESIZE_DEBOUNCE: Duration = Duration::from_millis(120);
const LOADING_DELAY: Duration = Duration::from_millis(90);

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// PDF or EPUB document to read.
    document: PathBuf,

    /// Page number to open (one-based; overrides saved state).
    #[arg(short = 'p', long = "page")]
    page: Option<usize>,

    /// Export one page as PNG and exit.
    #[arg(short = 'e', long)]
    export: bool,

    /// Start with inverted colours.
    #[arg(short = 'i', long)]
    invert: bool,

    /// Custom document black colour.
    #[arg(short = 'b', long = "black-color", default_value = "#000000")]
    black: String,

    /// Custom document white colour.
    #[arg(short = 'w', long = "white-color", default_value = "#ffffff")]
    white: String,

    /// Validate the Vivid path without connecting to a presenter.
    #[arg(long)]
    dry_run: bool,

    /// Write Vivid control and raster records to this directory.
    #[arg(long, value_name = "DIR")]
    trace: Option<PathBuf>,

    /// Print diagnostic logging (never includes credentials).
    #[arg(short, long)]
    verbose: bool,

    #[arg(long, hide = true)]
    probe_document: bool,
}

struct Runtime {
    app: App,
    viewport: WindowSize,
    current_image: Option<(usize, u64, Arc<PageImage>)>,
    pending_resize: Option<(u16, u16, Instant)>,
    interactive: bool,
    node_visible: bool,
    loading_deadline: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoadingPolicy {
    None,
    Immediate,
    Delayed,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    validate_cli(&cli)?;
    if cli.probe_document {
        let rendered = renderer::render_page(
            &cli.document,
            cli.page.unwrap_or(1) - 1,
            WindowSize::from_cells(80, 24, 10, 20),
        )?;
        let _ = (rendered.page.width, rendered.page_num, rendered.n_pages);
        return Ok(());
    }
    probe_document(&cli.document)?;

    let black = parse_color(&cli.black)?;
    let white = parse_color(&cli.white)?;
    let saved = state::load_state(&cli.document).unwrap_or_default();
    let initial_page = cli.page.map(|page| page - 1).unwrap_or(saved.page);

    if cli.export {
        let viewport = WindowSize::from_cells(120, 41, 10, 20);
        let mut options = RenderOptions::for_viewport(viewport, 1);
        options.rotation = saved.rotation;
        options.inverted = cli.invert || saved.inverted;
        options.tinted = saved.tinted;
        options.black = black;
        options.white = white;
        options.epub_font_size = saved.epub_font_size.unwrap_or(11.0);
        let n_pages =
            renderer::document_page_count(&cli.document, viewport, options.epub_font_size)?;
        let page = initial_page.min(n_pages.saturating_sub(1));
        let output = export::next_export_path(&cli.document, page, n_pages)?;
        renderer::export_document_page(
            &cli.document,
            page,
            viewport,
            &options,
            &output,
            saved.auto_crop,
        )?;
        println!("{}", output.display());
        return Ok(());
    }

    if let Some(trace_dir) = &cli.trace {
        std::fs::create_dir_all(trace_dir)
            .with_context(|| format!("cannot create trace directory {}", trace_dir.display()))?;
    }
    let _logger =
        flexi_logger::Logger::try_with_str(if cli.verbose { "debug" } else { "warn" })?.start()?;
    let session = ProducerSession::connect(&producer_config(&cli))
        .map_err(|error| anyhow::anyhow!("cannot connect to Vivid presenter: {error}"))?;
    let viewport = WindowSize::current(session.display_state())?;

    let mut app = App::new(initial_page);
    app.rotation = saved.rotation;
    app.inverted = cli.invert || saved.inverted;
    app.tinted = saved.tinted;
    app.auto_crop = saved.auto_crop;
    app.epub_font_size = saved.epub_font_size.unwrap_or(11.0);
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let mut runtime = Runtime {
        app,
        viewport,
        current_image: None,
        pending_resize: None,
        interactive,
        node_visible: true,
        loading_deadline: None,
    };

    let old_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        terminal::reset_terminal();
        old_hook(info);
    }));
    let _terminal = interactive
        .then(terminal::TerminalGuard::enter)
        .transpose()?;
    if interactive {
        terminal::clear_page_area(viewport)?;
    }

    let vivid = VividThread::spawn(session, viewport);
    wait_for_presenter(&vivid)?;
    let render = RenderThread::spawn(cli.document.clone(), viewport);
    wait_for_document(&render, &mut runtime)?;
    request_render(&render, &vivid, &mut runtime, black, white, interactive)?;

    if interactive {
        run_event_loop(&cli.document, &render, &vivid, &mut runtime, black, white)?;
    } else {
        wait_for_noninteractive_frame(&render, &vivid, &mut runtime, black, white)?;
    }

    state::save_state(
        &cli.document,
        &state::SavedState {
            page: runtime.app.page,
            rotation: runtime.app.rotation,
            inverted: runtime.app.inverted,
            auto_crop: runtime.app.auto_crop,
            tinted: runtime.app.tinted,
            epub_font_size: matches!(
                runtime.app.document_kind,
                renderer::DocumentKind::Reflowable
            )
            .then_some(runtime.app.epub_font_size),
        },
    );
    render.shutdown();
    vivid.shutdown();
    Ok(())
}

fn validate_cli(cli: &Cli) -> anyhow::Result<()> {
    if cli.page == Some(0) {
        bail!("page numbers start at 1");
    }
    if !cli.document.is_file() {
        bail!("document does not exist: {}", cli.document.display());
    }
    Ok(())
}

fn probe_document(path: &Path) -> anyhow::Result<()> {
    let status = Command::new(std::env::current_exe()?)
        .arg("--probe-document")
        .arg(path)
        .env_remove("VIVID_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("cannot start document preflight")?;
    if !status.success() {
        bail!("MuPDF could not open {}", path.display());
    }
    Ok(())
}

fn producer_config(cli: &Cli) -> ProducerConfig {
    ProducerConfig {
        endpoint: std::env::var("VIVID_ENDPOINT").ok(),
        bulk_endpoint: std::env::var("VIVID_ENDPOINT_BULK").ok(),
        token: std::env::var("VIVID_TOKEN").ok(),
        dry_run: cli.dry_run,
        trace_dir: cli.trace.clone(),
        verbose: cli.verbose,
        producer: "vvrd".to_owned(),
        producer_version: env!("CARGO_PKG_VERSION").to_owned(),
        required_features: vec![
            messages::FEATURE_RASTER_RGBA8,
            messages::FEATURE_SCENE_TRANSACTIONS,
            messages::FEATURE_GRID_CELL_NODES,
            messages::FEATURE_CREDIT_FLOW_CONTROL,
        ],
        optional_features: vec![
            messages::FEATURE_ENCODED_IMAGE_V1,
            messages::FEATURE_RASTER_ZSTD_V1,
            messages::FEATURE_VISIBILITY_EVENTS_V1,
            messages::FEATURE_NODE_CLIP_RECT_V1,
        ],
    }
}

fn parse_color(value: &str) -> anyhow::Result<i32> {
    let color = csscolorparser::parse(value)
        .map_err(|error| anyhow::anyhow!("invalid color {value:?}: {error}"))?;
    let [red, green, blue, _] = color.to_rgba8();
    Ok(i32::from_be_bytes([0, red, green, blue]))
}

fn render_options(app: &App, viewport: WindowSize, black: i32, white: i32) -> RenderOptions {
    let (width_px, height_px) = app.render_area(viewport);
    RenderOptions {
        width_px,
        height_px,
        rotation: app.rotation,
        inverted: app.inverted,
        tinted: app.tinted,
        black,
        white,
        epub_font_size: app.epub_font_size,
        search_term: app.search_term.clone(),
        generation: app.generation,
    }
}

fn request_render(
    render: &RenderThread,
    vivid: &VividThread,
    runtime: &mut Runtime,
    black: i32,
    white: i32,
    draw_loading: bool,
) -> anyhow::Result<()> {
    match loading_policy(
        draw_loading,
        runtime.current_image.is_some(),
        current_image_is_ready(runtime),
    ) {
        LoadingPolicy::None => runtime.loading_deadline = None,
        LoadingPolicy::Immediate => {
            runtime.loading_deadline = None;
            hide_node(vivid, runtime)?;
            terminal::draw_loading(runtime.viewport)?;
            draw_status(runtime)?;
        }
        LoadingPolicy::Delayed => {
            // Keep the last committed page on screen. Most page turns complete inside this grace
            // period, so replacing the source becomes an atomic-looking transition instead of a
            // hide/blank/show sequence.
            runtime.loading_deadline = Some(Instant::now() + LOADING_DELAY);
        }
    }
    render.commands.send(RenderCmd::Render {
        page: runtime.app.page,
        options: render_options(&runtime.app, runtime.viewport, black, white),
    })?;
    Ok(())
}

fn loading_policy(
    draw_loading: bool,
    has_displayed_page: bool,
    page_is_ready: bool,
) -> LoadingPolicy {
    if !draw_loading || page_is_ready {
        LoadingPolicy::None
    } else if has_displayed_page {
        LoadingPolicy::Delayed
    } else {
        LoadingPolicy::Immediate
    }
}

fn current_image_is_ready(runtime: &Runtime) -> bool {
    runtime
        .current_image
        .as_ref()
        .is_some_and(|(page, generation, _)| {
            *page == runtime.app.page && *generation == runtime.app.generation
        })
}

fn hide_node(vivid: &VividThread, runtime: &mut Runtime) -> anyhow::Result<()> {
    if runtime.node_visible {
        vivid.commands.send(PresentCmd::SetVisible(false))?;
        runtime.node_visible = false;
    }
    Ok(())
}

fn wait_for_presenter(vivid: &VividThread) -> anyhow::Result<()> {
    match vivid.events.recv_timeout(Duration::from_secs(10))? {
        PresentEvent::Ready => Ok(()),
        PresentEvent::Error(error) | PresentEvent::SourceLost(error) => bail!("{error}"),
        event => bail!("presenter stopped during startup: {event:?}"),
    }
}

fn wait_for_document(render: &RenderThread, runtime: &mut Runtime) -> anyhow::Result<()> {
    match render.events.recv_timeout(Duration::from_secs(30))? {
        RenderEvent::Opened {
            kind,
            n_pages,
            toc,
            metadata,
        } => {
            runtime.app.set_document(kind, n_pages);
            runtime.app.toc = toc;
            runtime.app.metadata = metadata;
            Ok(())
        }
        RenderEvent::Error(error) => bail!("{error}"),
        _ => bail!("document renderer stopped during startup"),
    }
}

fn wait_for_noninteractive_frame(
    render: &RenderThread,
    vivid: &VividThread,
    runtime: &mut Runtime,
    black: i32,
    white: i32,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if let Ok(event) = render.events.recv_timeout(Duration::from_millis(50)) {
            handle_render_event(
                Path::new("document"),
                render,
                vivid,
                runtime,
                event,
                black,
                white,
            )?;
        }
        while let Ok(event) = vivid.events.try_recv() {
            if matches!(event, PresentEvent::FrameShown { .. }) {
                return Ok(());
            }
            handle_present_event(render, vivid, runtime, event, black, white)?;
        }
    }
    bail!("timed out waiting for the first frame")
}

fn run_event_loop(
    document: &Path,
    render: &RenderThread,
    vivid: &VividThread,
    runtime: &mut Runtime,
    black: i32,
    white: i32,
) -> anyhow::Result<()> {
    loop {
        while let Ok(render_event) = render.events.try_recv() {
            handle_render_event(document, render, vivid, runtime, render_event, black, white)?;
        }
        while let Ok(present_event) = vivid.events.try_recv() {
            handle_present_event(render, vivid, runtime, present_event, black, white)?;
        }
        if let Some((cols, rows, deadline)) = runtime.pending_resize
            && Instant::now() >= deadline
        {
            runtime.pending_resize = None;
            runtime.viewport = WindowSize::from_cells(
                cols,
                rows,
                runtime.viewport.cell_width_px,
                runtime.viewport.cell_height_px,
            );
            runtime.app.invalidate();
            vivid.commands.send(PresentCmd::Resize(runtime.viewport))?;
            show_current(vivid, runtime)?;
            request_render(render, vivid, runtime, black, white, true)?;
        }
        if let Some(deadline) = runtime.loading_deadline
            && Instant::now() >= deadline
        {
            runtime.loading_deadline = None;
            if !current_image_is_ready(runtime)
                && matches!(runtime.app.input_mode, InputMode::Normal)
            {
                hide_node(vivid, runtime)?;
                terminal::draw_loading(runtime.viewport)?;
                draw_status(runtime)?;
            }
        }
        if event::poll(Duration::from_millis(20))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Release => {}
                Event::Key(key)
                    if handle_key(document, render, vivid, runtime, key, black, white)? =>
                {
                    return Ok(());
                }
                Event::Key(_) => {}
                Event::Resize(cols, rows) if cols > 0 && rows > 1 => {
                    runtime.pending_resize = Some((cols, rows, Instant::now() + RESIZE_DEBOUNCE));
                }
                Event::Mouse(mouse) => {
                    handle_mouse(render, vivid, runtime, mouse, black, white)?;
                }
                _ => {}
            }
        }
    }
}

fn handle_mouse(
    render: &RenderThread,
    vivid: &VividThread,
    runtime: &mut Runtime,
    mouse: MouseEvent,
    black: i32,
    white: i32,
) -> anyhow::Result<()> {
    let InputMode::Toc { selected } = runtime.app.input_mode else {
        return Ok(());
    };
    let next = match mouse.kind {
        MouseEventKind::ScrollDown => selected.saturating_add(1),
        MouseEventKind::ScrollUp => selected.saturating_sub(1),
        MouseEventKind::Down(MouseButton::Left) => {
            let start = selected.saturating_sub(runtime.viewport.page_rows() as usize - 1);
            start.saturating_add(mouse.row as usize)
        }
        _ => return Ok(()),
    }
    .min(runtime.app.toc.len().saturating_sub(1));
    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
        if let Some(page) = runtime.app.toc.get(next).map(|entry| entry.page) {
            runtime.app.input_mode = InputMode::Normal;
            runtime.app.go_to_page(page);
            request_render(render, vivid, runtime, black, white, true)?;
        }
    } else {
        runtime.app.input_mode = InputMode::Toc { selected: next };
        draw_overlay(vivid, runtime)?;
    }
    Ok(())
}

fn handle_render_event(
    document: &Path,
    render: &RenderThread,
    vivid: &VividThread,
    runtime: &mut Runtime,
    event: RenderEvent,
    black: i32,
    white: i32,
) -> anyhow::Result<()> {
    match event {
        RenderEvent::Opened {
            kind,
            n_pages,
            toc,
            metadata,
        } => {
            runtime.app.set_document(kind, n_pages);
            runtime.app.toc = toc;
            runtime.app.metadata = metadata;
        }
        RenderEvent::Page {
            page,
            generation,
            image,
        } if page == runtime.app.page && generation == runtime.app.generation => {
            runtime.current_image = Some((page, generation, Arc::new(image)));
            show_current(vivid, runtime)?;
        }
        RenderEvent::Page { .. } => {}
        RenderEvent::SearchComplete(counts) => {
            let total: usize = counts.iter().sum();
            runtime.app.set_search_counts(counts);
            runtime.app.show_info(format!("{total} search result(s)"));
            if runtime
                .app
                .search_counts
                .get(runtime.app.page)
                .and_then(|count| *count)
                .unwrap_or(0)
                == 0
            {
                let _ = runtime.app.next_search_result(false);
            }
            request_render(render, vivid, runtime, black, white, true)?;
        }
        RenderEvent::Links(links) => {
            runtime.app.input_mode = InputMode::Links {
                links,
                input: String::new(),
            };
            draw_overlay(vivid, runtime)?;
        }
        RenderEvent::Exported(path) => {
            runtime
                .app
                .show_info(format!("exported {}", path.display()));
            draw_status(runtime)?;
        }
        RenderEvent::Notice(notice) => {
            runtime.app.show_info(notice);
            draw_status(runtime)?;
        }
        RenderEvent::Error(error) => {
            runtime.app.show_info(error);
            draw_status(runtime)?;
        }
        RenderEvent::Stopped => bail!("document renderer stopped"),
    }
    let _ = document;
    Ok(())
}

fn handle_present_event(
    render: &RenderThread,
    vivid: &VividThread,
    runtime: &mut Runtime,
    event: PresentEvent,
    black: i32,
    white: i32,
) -> anyhow::Result<()> {
    match event {
        PresentEvent::Ready => {}
        PresentEvent::FrameShown {
            frame_id,
            content_width,
            content_height,
        } => {
            log::debug!("presented frame {frame_id}");
            runtime
                .app
                .set_rendered_size(content_width, content_height, runtime.viewport);
        }
        PresentEvent::Visibility(visible) => {
            if runtime.node_visible || visible {
                runtime.app.visible = visible;
            }
            if visible && matches!(runtime.app.input_mode, InputMode::Normal) {
                if runtime.current_image.is_some() {
                    show_current(vivid, runtime)?;
                } else {
                    request_render(render, vivid, runtime, black, white, true)?;
                }
            }
        }
        PresentEvent::SourceLost(error) => {
            runtime.app.show_info(format!("display recovered: {error}"));
            show_current(vivid, runtime)?;
            request_render(render, vivid, runtime, black, white, true)?;
        }
        PresentEvent::Error(error) => bail!("Vivid presenter error: {error}"),
        PresentEvent::Stopped => bail!("Vivid presenter connection closed"),
    }
    Ok(())
}

fn show_current(vivid: &VividThread, runtime: &mut Runtime) -> anyhow::Result<()> {
    if !matches!(runtime.app.input_mode, InputMode::Normal) {
        return Ok(());
    }
    if !runtime.app.visible {
        return Ok(());
    }
    let Some((_, _, image)) = &runtime.current_image else {
        return Ok(());
    };
    runtime.loading_deadline = None;
    if runtime.interactive {
        terminal::clear_page_area(runtime.viewport)?;
    }
    if !runtime.node_visible {
        vivid.commands.send(PresentCmd::SetVisible(true))?;
        runtime.node_visible = true;
    }
    vivid.commands.send(PresentCmd::ShowView {
        image: Arc::clone(image),
        viewport: runtime.viewport,
        transform: ViewTransform {
            offset_x: runtime.app.pan_x,
            offset_y: runtime.app.scroll_y,
            auto_crop: runtime.app.auto_crop,
        },
    })?;
    draw_status(runtime)
}

fn draw_status(runtime: &Runtime) -> anyhow::Result<()> {
    if !runtime.interactive {
        return Ok(());
    }
    terminal::draw_status(
        runtime.viewport,
        &runtime
            .app
            .msg
            .text(runtime.app.page, runtime.app.n_pages, runtime.app.zoom_mode),
    )
}

fn draw_overlay(vivid: &VividThread, runtime: &mut Runtime) -> anyhow::Result<()> {
    vivid.commands.send(PresentCmd::SetVisible(false))?;
    runtime.node_visible = false;
    match &runtime.app.input_mode {
        InputMode::Toc { selected } => terminal::draw_toc(
            runtime.viewport,
            &runtime.app.toc,
            *selected,
            runtime.app.page,
        )?,
        InputMode::Metadata => terminal::draw_metadata(runtime.viewport, &runtime.app.metadata)?,
        InputMode::Links { links, input } => {
            terminal::draw_links(runtime.viewport, links, input, runtime.app.page)?
        }
        InputMode::Help => terminal::draw_help(runtime.viewport)?,
        InputMode::Normal | InputMode::GoToPage(_) | InputMode::Search(_) => {}
    }
    draw_status(runtime)
}

fn close_overlay(vivid: &VividThread, runtime: &mut Runtime) -> anyhow::Result<()> {
    runtime.app.input_mode = InputMode::Normal;
    runtime.app.msg = StatusMsg::Hint;
    show_current(vivid, runtime)
}

fn handle_key(
    document: &Path,
    render: &RenderThread,
    vivid: &VividThread,
    runtime: &mut Runtime,
    key: KeyEvent,
    black: i32,
    white: i32,
) -> anyhow::Result<bool> {
    match runtime.app.input_mode.clone() {
        InputMode::GoToPage(mut input) => {
            match key.code {
                KeyCode::Esc => close_overlay(vivid, runtime)?,
                KeyCode::Backspace => {
                    input.pop();
                    runtime.app.input_mode = InputMode::GoToPage(input.clone());
                    runtime.app.show_info(format!("go to page: {input}"));
                    draw_status(runtime)?;
                }
                KeyCode::Char(character) if character.is_ascii_digit() => {
                    input.push(character);
                    runtime.app.input_mode = InputMode::GoToPage(input.clone());
                    runtime.app.show_info(format!("go to page: {input}"));
                    draw_status(runtime)?;
                }
                KeyCode::Enter => {
                    runtime.app.input_mode = InputMode::Normal;
                    if let Ok(page) = input.parse::<usize>()
                        && page > 0
                        && runtime.app.go_to_page(page - 1)
                    {
                        request_render(render, vivid, runtime, black, white, true)?;
                    }
                }
                _ => {}
            }
            return Ok(false);
        }
        InputMode::Search(mut input) => {
            match key.code {
                KeyCode::Esc => close_overlay(vivid, runtime)?,
                KeyCode::Backspace => {
                    input.pop();
                    runtime.app.input_mode = InputMode::Search(input.clone());
                    runtime.app.show_info(format!("/{input}"));
                    draw_status(runtime)?;
                }
                KeyCode::Char(character) => {
                    input.push(character);
                    runtime.app.input_mode = InputMode::Search(input.clone());
                    runtime.app.show_info(format!("/{input}"));
                    draw_status(runtime)?;
                }
                KeyCode::Enter => {
                    runtime.app.input_mode = InputMode::Normal;
                    runtime.app.search_term = (!input.is_empty()).then_some(input.clone());
                    runtime.app.clear_search_results();
                    if input.is_empty() {
                        runtime.app.msg = StatusMsg::Hint;
                        request_render(render, vivid, runtime, black, white, true)?;
                    } else {
                        runtime.app.show_info("searching...");
                        render.commands.send(RenderCmd::Search(input))?;
                    }
                }
                _ => {}
            }
            return Ok(false);
        }
        InputMode::Toc { mut selected } => {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q' | 't') => close_overlay(vivid, runtime)?,
                KeyCode::Down | KeyCode::Char('j') => {
                    selected = selected
                        .saturating_add(1)
                        .min(runtime.app.toc.len().saturating_sub(1));
                    runtime.app.input_mode = InputMode::Toc { selected };
                    draw_overlay(vivid, runtime)?;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    selected = selected.saturating_sub(1);
                    runtime.app.input_mode = InputMode::Toc { selected };
                    draw_overlay(vivid, runtime)?;
                }
                KeyCode::Enter => {
                    let target = runtime.app.toc.get(selected).map(|entry| entry.page);
                    runtime.app.input_mode = InputMode::Normal;
                    if let Some(target) = target {
                        runtime.app.go_to_page(target);
                        request_render(render, vivid, runtime, black, white, true)?;
                    }
                }
                _ => {}
            }
            return Ok(false);
        }
        InputMode::Metadata | InputMode::Help => {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('q' | '?' | 'M')) {
                close_overlay(vivid, runtime)?;
            }
            return Ok(false);
        }
        InputMode::Links { links, mut input } => {
            match key.code {
                KeyCode::Esc => close_overlay(vivid, runtime)?,
                KeyCode::Backspace => {
                    input.pop();
                    runtime.app.input_mode = InputMode::Links { links, input };
                    draw_overlay(vivid, runtime)?;
                }
                KeyCode::Char(character) if character.is_ascii_digit() => {
                    input.push(character);
                    runtime.app.input_mode = InputMode::Links { links, input };
                    draw_overlay(vivid, runtime)?;
                }
                KeyCode::Enter => {
                    let selected = input
                        .parse::<usize>()
                        .ok()
                        .and_then(|number| links.get(number.saturating_sub(1)))
                        .cloned();
                    runtime.app.input_mode = InputMode::Normal;
                    if let Some(link) = selected {
                        if let Some(page) = link.page {
                            runtime.app.go_to_page(page);
                            request_render(render, vivid, runtime, black, white, true)?;
                        } else {
                            open_url(&link.uri);
                            runtime.app.show_info(format!("opened {}", link.uri));
                            show_current(vivid, runtime)?;
                        }
                    } else {
                        show_current(vivid, runtime)?;
                    }
                }
                _ => {}
            }
            return Ok(false);
        }
        InputMode::Normal => {}
    }

    if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
        || key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
    {
        return Ok(true);
    }

    #[cfg(unix)]
    if key.code == KeyCode::Char('z') && key.modifiers.contains(KeyModifiers::CONTROL) {
        vivid.commands.send(PresentCmd::SetVisible(false))?;
        runtime.node_visible = false;
        terminal::suspend_and_resume()?;
        show_current(vivid, runtime)?;
        return Ok(false);
    }

    let scroll_step = runtime.viewport.cell_height_px.saturating_mul(3);
    let pan_step = runtime.viewport.cell_width_px.saturating_mul(4);
    let mut recompose = false;
    let mut rerender = false;
    match key.code {
        KeyCode::Char(' ') | KeyCode::Right => rerender = runtime.app.next_page(),
        KeyCode::Left => rerender = runtime.app.prev_page(),
        KeyCode::Char('j') => rerender = runtime.app.next_page(),
        KeyCode::Char('k') => rerender = runtime.app.prev_page(),
        KeyCode::Char('l') if runtime.app.zoom_mode => {
            recompose = runtime.app.pan_right(runtime.viewport, pan_step)
        }
        KeyCode::Char('h') if runtime.app.zoom_mode => recompose = runtime.app.pan_left(pan_step),
        KeyCode::Char('l') => rerender = runtime.app.next_page(),
        KeyCode::Char('h') => rerender = runtime.app.prev_page(),
        KeyCode::Down => match runtime.app.scroll_down(runtime.viewport, scroll_step) {
            ScrollAction::Scrolled => recompose = true,
            ScrollAction::TurnNext => rerender = runtime.app.next_page(),
            _ => {}
        },
        KeyCode::Up => match runtime.app.scroll_up(scroll_step) {
            ScrollAction::Scrolled => recompose = true,
            ScrollAction::TurnPrev => rerender = runtime.app.prev_page_at_bottom(runtime.viewport),
            _ => {}
        },
        KeyCode::PageDown => {
            if runtime.app.zoom_mode {
                match runtime.app.scroll_down(
                    runtime.viewport,
                    runtime
                        .viewport
                        .page_area_height_px()
                        .saturating_sub(runtime.viewport.cell_height_px),
                ) {
                    ScrollAction::Scrolled => recompose = true,
                    ScrollAction::TurnNext => rerender = runtime.app.next_page(),
                    _ => {}
                }
            } else {
                rerender = runtime.app.next_page();
            }
        }
        KeyCode::PageUp => {
            if runtime.app.zoom_mode {
                match runtime.app.scroll_up(
                    runtime
                        .viewport
                        .page_area_height_px()
                        .saturating_sub(runtime.viewport.cell_height_px),
                ) {
                    ScrollAction::Scrolled => recompose = true,
                    ScrollAction::TurnPrev => {
                        rerender = runtime.app.prev_page_at_bottom(runtime.viewport)
                    }
                    _ => {}
                }
            } else {
                rerender = runtime.app.prev_page();
            }
        }
        KeyCode::Char('z') if runtime.app.supports_zoom() => {
            runtime.app.toggle_zoom();
            rerender = true;
        }
        KeyCode::Char('o') if runtime.app.zoom_mode => rerender = runtime.app.zoom_in(),
        KeyCode::Char('O') if runtime.app.zoom_mode => {
            rerender = runtime.app.zoom_out(runtime.viewport)
        }
        KeyCode::Char('r') => {
            runtime.app.rotation = (runtime.app.rotation + 90) % 360;
            runtime.app.invalidate();
            rerender = true;
        }
        KeyCode::Char('i') => {
            runtime.app.inverted = !runtime.app.inverted;
            runtime.app.invalidate();
            rerender = true;
        }
        KeyCode::Char('d') => {
            runtime.app.tinted = !runtime.app.tinted;
            runtime.app.invalidate();
            rerender = true;
        }
        KeyCode::Char('c') => {
            runtime.app.auto_crop = !runtime.app.auto_crop;
            runtime.app.invalidate();
            recompose = true;
        }
        KeyCode::Char('<')
            if matches!(
                runtime.app.document_kind,
                renderer::DocumentKind::Reflowable
            ) =>
        {
            runtime.app.epub_font_size = (runtime.app.epub_font_size - 1.0).max(9.0);
            runtime.app.invalidate();
            rerender = true;
        }
        KeyCode::Char('>')
            if matches!(
                runtime.app.document_kind,
                renderer::DocumentKind::Reflowable
            ) =>
        {
            runtime.app.epub_font_size = (runtime.app.epub_font_size + 1.0).min(18.0);
            runtime.app.invalidate();
            rerender = true;
        }
        KeyCode::Char('g') => {
            runtime.app.input_mode = InputMode::GoToPage(String::new());
            runtime.app.show_info("go to page: ");
            draw_status(runtime)?;
        }
        KeyCode::Char('/') => {
            runtime.app.input_mode = InputMode::Search(String::new());
            runtime.app.show_info("/");
            draw_status(runtime)?;
        }
        KeyCode::Char('n') => rerender = runtime.app.next_search_result(false),
        KeyCode::Char('N') => rerender = runtime.app.next_search_result(true),
        KeyCode::Char('t') if !runtime.app.toc.is_empty() => {
            runtime.app.input_mode = InputMode::Toc { selected: 0 };
            draw_overlay(vivid, runtime)?;
        }
        KeyCode::Char('M') if !runtime.app.metadata.is_empty() => {
            runtime.app.input_mode = InputMode::Metadata;
            draw_overlay(vivid, runtime)?;
        }
        KeyCode::Char('f') => {
            runtime.app.show_info("extracting links...");
            draw_status(runtime)?;
            render
                .commands
                .send(RenderCmd::GetLinks(runtime.app.page))?;
        }
        KeyCode::Char('e') => {
            let output = export::next_export_path(document, runtime.app.page, runtime.app.n_pages)?;
            render.commands.send(RenderCmd::Export {
                page: runtime.app.page,
                output,
                options: render_options(&runtime.app, runtime.viewport, black, white),
                auto_crop: runtime.app.auto_crop,
            })?;
            runtime.app.show_info("exporting...");
            draw_status(runtime)?;
        }
        KeyCode::Char('R') | KeyCode::F(5) => {
            runtime.app.invalidate();
            render.commands.send(RenderCmd::ClearCache)?;
            rerender = true;
        }
        KeyCode::Char('?') => {
            runtime.app.input_mode = InputMode::Help;
            draw_overlay(vivid, runtime)?;
        }
        _ => {}
    }
    if rerender {
        request_render(render, vivid, runtime, black, white, true)?;
    } else if recompose {
        show_current(vivid, runtime)?;
    }
    Ok(false)
}

fn open_url(url: &str) {
    #[cfg(target_os = "macos")]
    let command = "open";
    #[cfg(not(target_os = "macos"))]
    let command = "xdg-open";
    let _ = Command::new(command)
        .arg(url)
        .env_remove("VIVID_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_css_colors_for_mupdf() {
        assert_eq!(
            parse_color("#123456").unwrap(),
            i32::from_be_bytes([0, 0x12, 0x34, 0x56])
        );
    }

    #[test]
    fn producer_advertises_canonical_feature_sets() {
        let cli = Cli::try_parse_from(["vvrd", "--dry-run", "doc.pdf"]).unwrap();
        let config = producer_config(&cli);
        assert_eq!(
            config.required_features,
            vec![
                messages::FEATURE_RASTER_RGBA8,
                messages::FEATURE_SCENE_TRANSACTIONS,
                messages::FEATURE_GRID_CELL_NODES,
                messages::FEATURE_CREDIT_FLOW_CONTROL,
            ]
        );
        assert_eq!(
            config.optional_features,
            vec![
                messages::FEATURE_ENCODED_IMAGE_V1,
                messages::FEATURE_RASTER_ZSTD_V1,
                messages::FEATURE_VISIBILITY_EVENTS_V1,
                messages::FEATURE_NODE_CLIP_RECT_V1,
            ]
        );
        for features in [&config.required_features, &config.optional_features] {
            assert!(features.windows(2).all(|pair| pair[0] < pair[1]));
        }
        assert!(
            config
                .required_features
                .iter()
                .all(|feature| config.optional_features.binary_search(feature).is_err())
        );
    }

    #[test]
    fn page_turn_retains_the_previous_frame_during_loading_grace() {
        assert_eq!(loading_policy(true, true, false), LoadingPolicy::Delayed);
        assert_eq!(loading_policy(true, false, false), LoadingPolicy::Immediate);
        assert_eq!(loading_policy(true, true, true), LoadingPolicy::None);
        assert_eq!(loading_policy(false, true, false), LoadingPolicy::None);
    }
}
