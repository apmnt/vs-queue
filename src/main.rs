use anyhow::{Context, Result};
use chrono::{Datelike, Duration, Local, NaiveDate, NaiveDateTime, Timelike};
use clap::Parser;
use crossterm::{
    cursor::{Hide, Show},
    event::{self, Event as CEvent, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols,
    text::{Line, Span},
    widgets::{
        Block, Borders, List, ListItem, Paragraph, Wrap,
        canvas::{Canvas, Line as CanvasLine, Points as CanvasPoints},
    },
};
use regex::Regex;
use std::collections::VecDeque;
use std::fs::{File, OpenOptions, create_dir_all};
use std::io::{self, BufRead, BufReader, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Duration as StdDuration;

const STATUS_HEIGHT: u16 = 8;
const OBSERVED_COLOR: Color = Color::Cyan;
const PROJECTION_COLOR: Color = Color::Yellow;

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Track queue progress from a log file, optionally launching the source process in detached mode."
)]
struct Cli {
    #[arg(
        long,
        value_name = "LOG_FILE",
        conflicts_with = "executable",
        help = "Watch an existing log file instead of launching a new process."
    )]
    watch: Option<PathBuf>,
    #[arg(
        value_name = "EXECUTABLE",
        required_unless_present = "watch",
        conflicts_with = "watch"
    )]
    executable: Option<PathBuf>,
    #[arg(
        value_name = "ARG",
        trailing_var_arg = true,
        allow_hyphen_values = true,
        requires = "executable",
        help = "Arguments passed through to the target executable. Use `--` before the child args."
    )]
    args: Vec<String>,
    #[arg(
        long,
        value_name = "LOG_FILE",
        requires = "executable",
        help = "Log file used for child stdout/stderr. Defaults to ./vs-queue-<executable>-<timestamp>.log"
    )]
    log_file: Option<PathBuf>,
    #[arg(
        long,
        default_value_t = 20,
        help = "How many queue samples to use in the rolling regression."
    )]
    regression_samples: usize,
    #[arg(
        long,
        default_value_t = 8,
        help = "How many recent log lines to keep visible in the live UI."
    )]
    recent_logs: usize,
    #[arg(
        long,
        default_value_t = 250,
        help = "UI refresh interval in milliseconds."
    )]
    refresh_ms: u64,
}

#[derive(Clone, Debug)]
struct QueueSample {
    timestamp: NaiveDateTime,
    position: f64,
}

#[derive(Clone, Debug)]
struct Estimate {
    eta: NaiveDateTime,
    remaining: Duration,
    positions_per_minute: f64,
    finish_elapsed_seconds: f64,
}

struct ChartData {
    observed: Vec<(f64, f64)>,
    projection: Vec<(f64, f64)>,
    x_bounds: [f64; 2],
    y_bounds: [f64; 2],
    x_labels: [String; 3],
    y_ticks: [f64; 3],
    y_labels: [String; 3],
}

enum SessionKind {
    Launch {
        executable: PathBuf,
        args: Vec<String>,
    },
    Watch,
}

impl SessionKind {
    fn description(&self) -> String {
        match self {
            Self::Launch { executable, args } => format_command(executable, args),
            Self::Watch => "watching existing log file".to_string(),
        }
    }

    fn tracker_hint(&self) -> &'static str {
        match self {
            Self::Launch { .. } => {
                "q / Esc / Ctrl+C stops only vs-queue; the launched process keeps running"
            }
            Self::Watch => "q / Esc / Ctrl+C stops the tracker",
        }
    }
}

struct AppState {
    session: SessionKind,
    log_file: PathBuf,
    output_is_tty: bool,
    max_recent_logs: usize,
    recent_logs: VecDeque<String>,
    samples: Vec<QueueSample>,
    max_seen_position: f64,
    projected_curve: Vec<(f64, f64)>,
}

impl AppState {
    fn new(session: SessionKind, log_file: PathBuf, cli: &Cli, output_is_tty: bool) -> Self {
        Self {
            session,
            log_file,
            output_is_tty,
            max_recent_logs: cli.recent_logs,
            recent_logs: VecDeque::with_capacity(cli.recent_logs),
            samples: Vec::new(),
            max_seen_position: 0.0,
            projected_curve: Vec::new(),
        }
    }

    fn record_line(&mut self, line: &str) -> Option<QueueSample> {
        self.push_recent(line.to_string());

        let mut parsed = parse_queue_sample(line)?;
        let received_at = Local::now().naive_local();
        if let Some(previous) = self.samples.last() {
            let is_liveish = (received_at - parsed.timestamp)
                .num_seconds()
                .unsigned_abs()
                <= 2;
            parsed.timestamp = if parsed.timestamp > previous.timestamp {
                parsed.timestamp
            } else if is_liveish {
                received_at.max(previous.timestamp + Duration::milliseconds(1))
            } else {
                previous.timestamp + Duration::milliseconds(1)
            };
        }

        self.max_seen_position = self.max_seen_position.max(parsed.position);
        self.samples.push(parsed.clone());
        Some(parsed)
    }

    fn push_recent(&mut self, line: String) {
        if self.max_recent_logs == 0 {
            return;
        }

        if self.recent_logs.len() == self.max_recent_logs {
            self.recent_logs.pop_front();
        }

        self.recent_logs.push_back(line);
    }

    fn refresh_projection(&mut self, estimate: Option<&Estimate>) {
        let Some(first) = self.samples.first() else {
            return;
        };

        if let Some(estimate) = estimate {
            let observed: Vec<(f64, f64)> = self
                .samples
                .iter()
                .map(|sample| {
                    (
                        elapsed_seconds(first.timestamp, sample.timestamp),
                        sample.position,
                    )
                })
                .collect();
            if let Some(curve) = build_projection_curve(&observed, estimate.finish_elapsed_seconds)
            {
                self.projected_curve = curve;
            }
        }
    }
}

struct LogFollower {
    reader: BufReader<File>,
    line_buffer: String,
}

impl LogFollower {
    fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .open(path)
            .with_context(|| format!("failed to open log file {}", path.display()))?;

        Ok(Self {
            reader: BufReader::new(file),
            line_buffer: String::new(),
        })
    }

    fn read_next_line(&mut self) -> io::Result<Option<String>> {
        self.line_buffer.clear();
        let bytes = self.reader.read_line(&mut self.line_buffer)?;
        if bytes == 0 {
            return Ok(None);
        }

        while matches!(self.line_buffer.chars().last(), Some('\n' | '\r')) {
            self.line_buffer.pop();
        }

        Ok(Some(self.line_buffer.clone()))
    }
}

struct Tui {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}

impl Tui {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("failed to enable raw mode")?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, Hide)
            .context("failed to enter the alternate screen")?;

        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend).context("failed to initialize the TUI")?;
        terminal.clear().context("failed to clear the terminal")?;
        Ok(Self { terminal })
    }

    fn draw(
        &mut self,
        state: &AppState,
        estimate: Option<&Estimate>,
        exit_status: Option<ExitStatus>,
    ) -> Result<()> {
        self.terminal
            .draw(|frame| render_tui(frame, state, estimate, exit_status))
            .context("failed to draw the TUI")?;
        Ok(())
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = self.terminal.show_cursor();
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, Show, LeaveAlternateScreen);
    }
}

fn main() -> Result<()> {
    run(Cli::parse())
}

fn run(cli: Cli) -> Result<()> {
    let interrupted = Arc::new(AtomicBool::new(false));
    ctrlc::set_handler({
        let interrupted = Arc::clone(&interrupted);
        move || {
            interrupted.store(true, Ordering::SeqCst);
        }
    })
    .context("failed to install the Ctrl-C handler")?;

    let output_is_tty = io::stdout().is_terminal();

    match (&cli.watch, &cli.executable) {
        (Some(log_file), None) => {
            let state = AppState::new(SessionKind::Watch, log_file.clone(), &cli, output_is_tty);
            track_loop(state, None, &cli, interrupted)
        }
        (None, Some(executable)) => {
            let log_file = cli
                .log_file
                .clone()
                .unwrap_or_else(|| default_log_path(executable));
            let child = launch_child_to_log(executable, &cli.args, &log_file)?;
            let state = AppState::new(
                SessionKind::Launch {
                    executable: executable.clone(),
                    args: cli.args.clone(),
                },
                log_file,
                &cli,
                output_is_tty,
            );
            track_loop(state, Some(child), &cli, interrupted)
        }
        _ => unreachable!("clap enforces either --watch or an executable"),
    }
}

fn track_loop(
    mut state: AppState,
    mut child: Option<Child>,
    cli: &Cli,
    interrupted: Arc<AtomicBool>,
) -> Result<()> {
    let mut follower = LogFollower::open(&state.log_file)?;
    let refresh_interval = StdDuration::from_millis(cli.refresh_ms.max(50));
    let mut exit_status = None;
    let mut stopped_by_user = false;
    let keep_open_after_process_exit = state.output_is_tty;
    let mut tui = if state.output_is_tty {
        Some(Tui::enter()?)
    } else {
        print_start_message(&state);
        None
    };

    let mut estimate = compute_estimate(&state.samples, cli.regression_samples);
    state.refresh_projection(estimate.as_ref());
    if let Some(tui) = tui.as_mut() {
        tui.draw(&state, estimate.as_ref(), exit_status)?;
    }

    loop {
        let mut saw_new_lines = false;
        drain_new_lines(&mut follower, &mut state, cli, &mut saw_new_lines)?;

        if let Some(child) = child.as_mut() {
            if exit_status.is_none() {
                exit_status = child
                    .try_wait()
                    .context("failed while polling the detached child process")?;
            }
        }

        if exit_status.is_some() {
            drain_new_lines(&mut follower, &mut state, cli, &mut saw_new_lines)?;
        }

        estimate = compute_estimate(&state.samples, cli.regression_samples);
        state.refresh_projection(estimate.as_ref());

        if let Some(tui) = tui.as_mut() {
            tui.draw(&state, estimate.as_ref(), exit_status)?;
        }

        if state.output_is_tty {
            let timeout = if saw_new_lines {
                StdDuration::from_millis(10)
            } else {
                refresh_interval
            };
            if interrupted.load(Ordering::SeqCst) || should_stop_tui(timeout)? {
                stopped_by_user = true;
                if matches!(state.session, SessionKind::Launch { .. }) && exit_status.is_none() {
                    state.push_recent(
                        "[vs-queue] tracker stopped; launched process keeps running and logging"
                            .to_string(),
                    );
                    estimate = compute_estimate(&state.samples, cli.regression_samples);
                    state.refresh_projection(estimate.as_ref());
                    if let Some(tui) = tui.as_mut() {
                        tui.draw(&state, estimate.as_ref(), exit_status)?;
                    }
                }
                break;
            }
        } else {
            if interrupted.load(Ordering::SeqCst) {
                stopped_by_user = true;
                break;
            }

            if exit_status.is_none() {
                thread::sleep(refresh_interval);
            }
        }

        if exit_status.is_some() && !keep_open_after_process_exit {
            break;
        }
    }

    drop(tui);

    if stopped_by_user {
        println!("tracker stopped");
        if let SessionKind::Launch { .. } = state.session {
            if exit_status.is_none() {
                println!("launched process continues in the background");
                println!("log file: {}", state.log_file.display());
            } else {
                println!("process {}", format_exit_status(exit_status));
            }
        }
    } else if child.is_some() {
        println!("process {}", format_exit_status(exit_status));
    }

    Ok(())
}

fn should_stop_tui(timeout: StdDuration) -> Result<bool> {
    if !event::poll(timeout).context("failed while polling terminal input")? {
        return Ok(false);
    }

    match event::read().context("failed while reading terminal input")? {
        CEvent::Key(key) => Ok(is_exit_key(key)),
        _ => Ok(false),
    }
}

fn is_exit_key(key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Esc => true,
        KeyCode::Char('q') | KeyCode::Char('Q') => true,
        KeyCode::Char('c') | KeyCode::Char('C') => key.modifiers.contains(KeyModifiers::CONTROL),
        _ => false,
    }
}

fn drain_new_lines(
    follower: &mut LogFollower,
    state: &mut AppState,
    cli: &Cli,
    saw_new_lines: &mut bool,
) -> Result<()> {
    while let Some(line) = follower
        .read_next_line()
        .with_context(|| format!("failed while reading log file {}", state.log_file.display()))?
    {
        *saw_new_lines = true;
        let sample = state.record_line(&line);
        if !state.output_is_tty {
            println!("{line}");
            if sample.is_some() {
                print_compact_estimate(state, cli);
            }
        }
    }

    Ok(())
}

fn print_start_message(state: &AppState) {
    match &state.session {
        SessionKind::Launch { executable, args } => {
            println!("launched {}", format_command(executable, args));
            println!("log file: {}", state.log_file.display());
            println!("Ctrl-C stops only the tracker; the launched process keeps running");
        }
        SessionKind::Watch => {
            println!("watching log file: {}", state.log_file.display());
        }
    }
}

fn launch_child_to_log(executable: &Path, args: &[String], log_file: &Path) -> Result<Child> {
    if let Some(parent) = log_file
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        create_dir_all(parent).with_context(|| {
            format!(
                "failed to create directories for log file {}",
                log_file.display()
            )
        })?;
    }

    let stdout_log = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(log_file)
        .with_context(|| format!("failed to create log file {}", log_file.display()))?;
    let stderr_log = stdout_log
        .try_clone()
        .with_context(|| format!("failed to clone log file handle {}", log_file.display()))?;

    let mut command = std::process::Command::new(executable);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_log))
        .stderr(Stdio::from(stderr_log));

    detach_command(&mut command);

    command
        .spawn()
        .with_context(|| format!("failed to launch {}", format_command(executable, args)))
}

#[cfg(unix)]
fn detach_command(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;

    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn detach_command(_command: &mut std::process::Command) {}

fn render_tui(
    frame: &mut Frame,
    state: &AppState,
    estimate: Option<&Estimate>,
    exit_status: Option<ExitStatus>,
) {
    let log_height = (state.max_recent_logs.max(3) as u16).saturating_add(2);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(STATUS_HEIGHT),
            Constraint::Min(10),
            Constraint::Length(log_height),
        ])
        .split(frame.area());

    let status = Paragraph::new(build_status_lines(state, estimate, exit_status))
        .block(Block::default().borders(Borders::ALL).title("Status"))
        .wrap(Wrap { trim: true });
    frame.render_widget(status, chunks[0]);

    let chart_block = Block::default()
        .borders(Borders::ALL)
        .title("Queue Position Over Time");
    let chart_inner = chart_block.inner(chunks[1]);
    frame.render_widget(chart_block, chunks[1]);

    if let Some(chart_data) = build_chart_data(state, estimate) {
        render_braille_chart(frame, chart_inner, &chart_data);
    } else {
        let placeholder = Paragraph::new("Waiting for a log line matching `... position: <n>`")
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true });
        frame.render_widget(placeholder, chart_inner);
    }

    let log_items: Vec<ListItem<'static>> = if state.recent_logs.is_empty() {
        vec![ListItem::new(Line::from("No log lines yet"))]
    } else {
        state
            .recent_logs
            .iter()
            .map(|line| ListItem::new(Line::from(line.clone())))
            .collect()
    };
    let logs =
        List::new(log_items).block(Block::default().borders(Borders::ALL).title("Recent Logs"));
    frame.render_widget(logs, chunks[2]);
}

fn build_status_lines(
    state: &AppState,
    estimate: Option<&Estimate>,
    exit_status: Option<ExitStatus>,
) -> Vec<Line<'static>> {
    let queue_line = if let Some(sample) = state.samples.last() {
        format!(
            "queue: {:.0}/{:.0}    progress: {:.1}%    process: {}",
            sample.position,
            state.max_seen_position.max(sample.position),
            progress_percent(sample.position, state.max_seen_position),
            format_exit_status(exit_status)
        )
    } else {
        format!(
            "queue: waiting for matching log lines    process: {}",
            format_exit_status(exit_status)
        )
    };

    let last_sample_line = if let Some(sample) = state.samples.last() {
        format!("last sample: {}", format_timestamp(sample.timestamp))
    } else {
        "last sample: n/a".to_string()
    };

    let eta_line = match estimate {
        Some(estimate) => format!(
            "eta: {}    remaining: {}    speed: {:.2} positions/min",
            format_timestamp(estimate.eta),
            format_duration(estimate.remaining),
            estimate.positions_per_minute
        ),
        None if state
            .samples
            .last()
            .map(|sample| sample.position <= 0.0)
            .unwrap_or(false) =>
        {
            format!(
                "eta: reached queue end at {}    remaining: 0s    speed: n/a",
                format_timestamp(state.samples.last().expect("sample exists").timestamp)
            )
        }
        None => "eta: collecting more data / queue not moving yet".to_string(),
    };

    vec![
        Line::from(vec![
            Span::styled("vs-queue", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("  "),
            Span::raw(state.session.tracker_hint()),
        ]),
        Line::from(format!("source: {}", state.session.description())),
        Line::from(format!("log file: {}", state.log_file.display())),
        Line::from(queue_line),
        Line::from(last_sample_line),
        Line::from(eta_line),
        Line::from("plot: braille canvas    x-axis: time    y-axis: queue position"),
    ]
}

fn render_braille_chart(frame: &mut Frame, area: Rect, chart_data: &ChartData) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(6),
            Constraint::Length(2),
        ])
        .split(area);

    let legend = Paragraph::new(Line::from(vec![
        Span::raw("legend: "),
        Span::styled(
            "real progress",
            Style::default()
                .fg(OBSERVED_COLOR)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            "projection",
            Style::default()
                .fg(PROJECTION_COLOR)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    frame.render_widget(legend, rows[0]);

    let y_label_width = chart_data
        .y_labels
        .iter()
        .map(|label| label.len())
        .max()
        .unwrap_or(1)
        .saturating_add(2) as u16;
    let plot_columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(y_label_width), Constraint::Min(10)])
        .split(rows[1]);
    let axis_style = Style::default().fg(Color::Gray);
    render_y_axis(frame, plot_columns[0], chart_data, axis_style);

    let canvas = Canvas::default()
        .marker(symbols::Marker::Braille)
        .x_bounds(chart_data.x_bounds)
        .y_bounds(chart_data.y_bounds)
        .paint(|ctx| {
            let axis_color = Color::DarkGray;
            ctx.draw(&CanvasLine::new(
                chart_data.x_bounds[0],
                chart_data.y_bounds[0],
                chart_data.x_bounds[1],
                chart_data.y_bounds[0],
                axis_color,
            ));

            for segment in chart_data.projection.windows(2) {
                ctx.draw(&CanvasLine::new(
                    segment[0].0,
                    segment[0].1,
                    segment[1].0,
                    segment[1].1,
                    PROJECTION_COLOR,
                ));
            }
            if !chart_data.projection.is_empty() {
                ctx.draw(&CanvasPoints::new(&chart_data.projection, PROJECTION_COLOR));
            }

            for segment in chart_data.observed.windows(2) {
                ctx.draw(&CanvasLine::new(
                    segment[0].0,
                    segment[0].1,
                    segment[1].0,
                    segment[1].1,
                    OBSERVED_COLOR,
                ));
            }
            ctx.draw(&CanvasPoints::new(&chart_data.observed, OBSERVED_COLOR));
        });
    frame.render_widget(canvas, plot_columns[1]);

    let x_axis_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(rows[2]);
    let x_axis_columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(34),
            Constraint::Percentage(32),
            Constraint::Percentage(34),
        ])
        .split(x_axis_rows[0]);
    frame.render_widget(
        Paragraph::new(chart_data.x_labels[0].clone()).style(axis_style),
        x_axis_columns[0],
    );
    frame.render_widget(
        Paragraph::new(chart_data.x_labels[1].clone())
            .alignment(Alignment::Center)
            .style(axis_style),
        x_axis_columns[1],
    );
    frame.render_widget(
        Paragraph::new(chart_data.x_labels[2].clone())
            .alignment(Alignment::Right)
            .style(axis_style),
        x_axis_columns[2],
    );
    frame.render_widget(
        Paragraph::new("time")
            .alignment(Alignment::Center)
            .style(axis_style.add_modifier(Modifier::BOLD)),
        x_axis_rows[1],
    );
}

fn render_y_axis(frame: &mut Frame, area: Rect, chart_data: &ChartData, style: Style) {
    let height = area.height.max(1) as usize;
    let width = area.width.max(2) as usize;
    let label_width = width.saturating_sub(1);
    let mut lines = vec![format!("{:>label_width$}│", "", label_width = label_width); height];

    for (tick, label) in chart_data.y_ticks.iter().zip(chart_data.y_labels.iter()) {
        let row = y_tick_row(*tick, chart_data.y_bounds, height);
        lines[row] = format!("{label:>label_width$}┤", label_width = label_width);
    }

    let axis = Paragraph::new(lines.join("\n")).style(style);
    frame.render_widget(axis, area);
}

fn y_tick_row(tick: f64, y_bounds: [f64; 2], height: usize) -> usize {
    if height <= 1 || (y_bounds[1] - y_bounds[0]).abs() <= f64::EPSILON {
        return 0;
    }

    let normalized = ((y_bounds[1] - tick) / (y_bounds[1] - y_bounds[0])).clamp(0.0, 1.0);
    (normalized * (height.saturating_sub(1) as f64)).round() as usize
}

fn build_chart_data(state: &AppState, _estimate: Option<&Estimate>) -> Option<ChartData> {
    let first = state.samples.first()?;
    let last = state.samples.last()?;
    let current_elapsed_seconds = elapsed_seconds(first.timestamp, last.timestamp);
    let observed: Vec<(f64, f64)> = state
        .samples
        .iter()
        .map(|sample| {
            (
                elapsed_seconds(first.timestamp, sample.timestamp),
                sample.position,
            )
        })
        .collect();

    let projection = state.projected_curve.clone();
    let x_max = projection
        .last()
        .map(|point| point.0)
        .unwrap_or(current_elapsed_seconds)
        .max(current_elapsed_seconds)
        .max(3.0);
    let y_max = (first.position.max(1.0) * 1.1).ceil();
    let chart_end_time = first.timestamp + Duration::milliseconds((x_max * 1000.0).round() as i64);
    let midpoint_time = first.timestamp + Duration::milliseconds((x_max * 500.0).round() as i64);
    let y_ticks = queue_axis_ticks(y_max);

    Some(ChartData {
        observed,
        projection,
        x_bounds: [0.0, x_max],
        y_bounds: [0.0, y_max],
        x_labels: [
            format_axis_time(first.timestamp),
            format_axis_time(midpoint_time),
            format_axis_time(chart_end_time),
        ],
        y_ticks,
        y_labels: queue_axis_labels(y_ticks),
    })
}

fn build_projection_curve(
    observed: &[(f64, f64)],
    finish_elapsed_seconds: f64,
) -> Option<Vec<(f64, f64)>> {
    let &(current_elapsed_seconds, _) = observed.last()?;
    let &(start_elapsed_seconds, start_position) = observed.first()?;
    if !finish_elapsed_seconds.is_finite()
        || finish_elapsed_seconds <= start_elapsed_seconds
        || current_elapsed_seconds < start_elapsed_seconds
    {
        return Some(observed.to_vec());
    }

    let (control_one, control_two) =
        fit_projection_controls(observed, start_position, finish_elapsed_seconds);
    let samples = 96_usize;
    let mut points = Vec::with_capacity(samples + 1);

    for index in 0..=samples {
        let progress = index as f64 / samples as f64;
        let x = finish_elapsed_seconds * progress;
        let y = cubic_bezier_value(progress, start_position, control_one, control_two, 0.0)
            .clamp(0.0, start_position.max(0.0));
        points.push((x, y));
    }

    Some(points)
}

fn fit_projection_controls(
    observed: &[(f64, f64)],
    start_position: f64,
    finish_elapsed_seconds: f64,
) -> (f64, f64) {
    let linear_control_one = start_position * (2.0 / 3.0);
    let linear_control_two = start_position * (1.0 / 3.0);
    if observed.len() < 4 || finish_elapsed_seconds <= f64::EPSILON {
        return (linear_control_one, linear_control_two);
    }

    let mut s11 = 0.0;
    let mut s12 = 0.0;
    let mut s22 = 0.0;
    let mut t1 = 0.0;
    let mut t2 = 0.0;
    let count = observed.len().saturating_sub(1).max(1) as f64;

    for (index, (elapsed_seconds, position)) in observed.iter().enumerate() {
        let u = (*elapsed_seconds / finish_elapsed_seconds).clamp(0.0, 1.0);
        let one_minus = 1.0 - u;
        let b0 = one_minus.powi(3);
        let b1 = 3.0 * one_minus.powi(2) * u;
        let b2 = 3.0 * one_minus * u.powi(2);
        let target = *position - (b0 * start_position);
        let weight = 1.0 + 3.0 * ((index as f64) / count).powi(2);

        s11 += weight * b1 * b1;
        s12 += weight * b1 * b2;
        s22 += weight * b2 * b2;
        t1 += weight * b1 * target;
        t2 += weight * b2 * target;
    }

    let determinant = (s11 * s22) - (s12 * s12);
    if determinant.abs() <= f64::EPSILON {
        return (linear_control_one, linear_control_two);
    }

    let mut control_one = ((t1 * s22) - (t2 * s12)) / determinant;
    let mut control_two = ((s11 * t2) - (s12 * t1)) / determinant;
    control_one = control_one.clamp(0.0, start_position.max(0.0));
    control_two = control_two.clamp(0.0, start_position.max(0.0));
    if control_one < control_two {
        std::mem::swap(&mut control_one, &mut control_two);
    }
    control_two = control_two.clamp(0.0, control_one);

    let blend = ((observed.len().saturating_sub(2) as f64) / 6.0).clamp(0.0, 1.0);
    (
        lerp(linear_control_one, control_one, blend),
        lerp(linear_control_two, control_two, blend),
    )
}

fn cubic_bezier_value(t: f64, p0: f64, p1: f64, p2: f64, p3: f64) -> f64 {
    let one_minus = 1.0 - t;
    (one_minus.powi(3) * p0)
        + (3.0 * one_minus.powi(2) * t * p1)
        + (3.0 * one_minus * t.powi(2) * p2)
        + (t.powi(3) * p3)
}

fn lerp(start: f64, end: f64, t: f64) -> f64 {
    start + ((end - start) * t)
}

fn queue_axis_ticks(y_max: f64) -> [f64; 3] {
    [y_max, y_max / 2.0, 0.0]
}

fn queue_axis_labels(y_ticks: [f64; 3]) -> [String; 3] {
    let midpoint = y_ticks[1].round();
    [
        format!("{:.0}", y_ticks[0]),
        if midpoint > 0.0 && midpoint < y_ticks[0].round() {
            format!("{midpoint:.0}")
        } else {
            String::new()
        },
        "0".to_string(),
    ]
}

fn compute_estimate(samples: &[QueueSample], regression_samples: usize) -> Option<Estimate> {
    if samples.len() < 2 {
        return None;
    }

    let first = samples.first()?;
    let current = samples.last()?;
    let window_len = regression_samples.max(2).min(samples.len());
    let window = &samples[samples.len() - window_len..];

    let points: Vec<(f64, f64)> = window
        .iter()
        .map(|sample| {
            (
                elapsed_seconds(first.timestamp, sample.timestamp),
                sample.position,
            )
        })
        .collect();

    let slope = regression_slope(&points)?;
    if slope >= -f64::EPSILON {
        return None;
    }

    let current_elapsed_seconds = elapsed_seconds(first.timestamp, current.timestamp);
    let remaining_seconds = current.position / -slope;
    if !remaining_seconds.is_finite() || remaining_seconds <= 0.0 {
        return None;
    }

    let remaining = Duration::milliseconds((remaining_seconds * 1000.0).round() as i64);
    let eta = current.timestamp + remaining;
    Some(Estimate {
        eta,
        remaining,
        positions_per_minute: -slope * 60.0,
        finish_elapsed_seconds: current_elapsed_seconds + remaining_seconds,
    })
}

fn regression_slope(points: &[(f64, f64)]) -> Option<f64> {
    let n = points.len() as f64;
    if n < 2.0 {
        return None;
    }

    let (sum_x, sum_y, sum_xx, sum_xy) = points.iter().fold(
        (0.0, 0.0, 0.0, 0.0),
        |(sum_x, sum_y, sum_xx, sum_xy), (x, y)| {
            (sum_x + x, sum_y + y, sum_xx + x * x, sum_xy + x * y)
        },
    );

    let denominator = (n * sum_xx) - (sum_x * sum_x);
    if denominator.abs() <= f64::EPSILON {
        return None;
    }

    Some(((n * sum_xy) - (sum_x * sum_y)) / denominator)
}

fn parse_queue_sample(line: &str) -> Option<QueueSample> {
    let captures = queue_line_regex().captures(line)?;
    let day = captures.name("day")?.as_str().parse::<u32>().ok()?;
    let month = captures.name("month")?.as_str().parse::<u32>().ok()?;
    let year = captures.name("year")?.as_str().parse::<i32>().ok()?;
    let hour = captures.name("hour")?.as_str().parse::<u32>().ok()?;
    let minute = captures.name("minute")?.as_str().parse::<u32>().ok()?;
    let second = captures.name("second")?.as_str().parse::<u32>().ok()?;
    let position = captures.name("position")?.as_str().parse::<f64>().ok()?;

    let date = NaiveDate::from_ymd_opt(year, month, day)?;
    let timestamp = date.and_hms_opt(hour, minute, second)?;

    Some(QueueSample {
        timestamp,
        position,
    })
}

fn queue_line_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(
            r"^(?P<day>\d{1,2})\.(?P<month>\d{1,2})\.(?P<year>\d{4})\s+(?P<hour>\d{1,2}):(?P<minute>\d{2}):(?P<second>\d{2}).*?position:\s*(?P<position>\d+)",
        )
        .expect("queue regex must compile")
    })
}

fn elapsed_seconds(start: NaiveDateTime, end: NaiveDateTime) -> f64 {
    (end - start).num_milliseconds() as f64 / 1000.0
}

fn progress_percent(current_position: f64, max_seen_position: f64) -> f64 {
    if max_seen_position <= 0.0 {
        return 0.0;
    }

    ((max_seen_position - current_position) / max_seen_position * 100.0).clamp(0.0, 100.0)
}

fn format_command(executable: &Path, args: &[String]) -> String {
    let mut parts = vec![shell_quote(&executable.to_string_lossy())];
    parts.extend(args.iter().map(|arg| shell_quote(arg)));
    parts.join(" ")
}

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '/' | '.' | ':'))
    {
        value.to_string()
    } else {
        format!("{value:?}")
    }
}

fn format_timestamp(timestamp: NaiveDateTime) -> String {
    format!(
        "{}.{}.{} {:02}:{:02}:{:02}",
        timestamp.day(),
        timestamp.month(),
        timestamp.year(),
        timestamp.hour(),
        timestamp.minute(),
        timestamp.second()
    )
}

fn format_axis_time(timestamp: NaiveDateTime) -> String {
    format!("{:02}:{:02}", timestamp.hour(), timestamp.minute())
}

fn format_duration(duration: Duration) -> String {
    let total_seconds = duration.num_seconds().max(0);
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;

    if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

fn format_exit_status(exit_status: Option<ExitStatus>) -> String {
    match exit_status {
        Some(status) if status.success() => "exited successfully".to_string(),
        Some(status) => match status.code() {
            Some(code) => format!("exited with code {code}"),
            None => "terminated by signal".to_string(),
        },
        None => "running / unknown".to_string(),
    }
}

fn print_compact_estimate(state: &AppState, cli: &Cli) {
    let Some(sample) = state.samples.last() else {
        return;
    };

    let progress = progress_percent(sample.position, state.max_seen_position);
    if sample.position <= 0.0 {
        println!(
            "queue=0 progress=100.0% reached={} remaining=0s",
            format_timestamp(sample.timestamp)
        );
    } else if let Some(estimate) = compute_estimate(&state.samples, cli.regression_samples) {
        println!(
            "queue={:.0} progress={:.1}% eta={} remaining={} rate={:.2}/min",
            sample.position,
            progress,
            format_timestamp(estimate.eta),
            format_duration(estimate.remaining),
            estimate.positions_per_minute
        );
    } else {
        println!(
            "queue={:.0} progress={:.1}% eta=pending",
            sample.position, progress
        );
    }
}

fn default_log_path(executable: &Path) -> PathBuf {
    let stem = executable
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("process");
    let sanitized_stem: String = stem
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '-'
            }
        })
        .collect();
    let now = Local::now();
    let filename = format!(
        "vs-queue-{}-{:04}{:02}{:02}-{:02}{:02}{:02}.log",
        sanitized_stem,
        now.year(),
        now.month(),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    );

    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(filename)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_queue_line() {
        let line =
            "6.4.2026 17:07:03 [Client Notification] Client is in connect queue at position: 90";
        let sample = parse_queue_sample(line).expect("line should parse");

        assert_eq!(sample.position, 90.0);
        assert_eq!(
            sample.timestamp,
            NaiveDate::from_ymd_opt(2026, 4, 6)
                .unwrap()
                .and_hms_opt(17, 7, 3)
                .unwrap()
        );
    }

    #[test]
    fn estimates_eta_from_linear_queue_progress() {
        let start = NaiveDate::from_ymd_opt(2026, 4, 6)
            .unwrap()
            .and_hms_opt(17, 0, 0)
            .unwrap();
        let samples = vec![
            QueueSample {
                timestamp: start,
                position: 100.0,
            },
            QueueSample {
                timestamp: start + Duration::minutes(1),
                position: 90.0,
            },
            QueueSample {
                timestamp: start + Duration::minutes(2),
                position: 80.0,
            },
        ];

        let estimate = compute_estimate(&samples, 20).expect("estimate should exist");
        assert_eq!(estimate.positions_per_minute.round(), 10.0);
        assert_eq!(estimate.remaining, Duration::minutes(8));
        assert_eq!(estimate.eta, start + Duration::minutes(10));
    }

    #[test]
    fn projection_curve_spans_from_start_to_finish() {
        let curve = build_projection_curve(&[(0.0, 100.0), (60.0, 90.0)], 600.0)
            .expect("curve should exist");

        let first = curve.first().expect("curve has start");
        let last = curve.last().expect("curve has end");
        assert_eq!(*first, (0.0, 100.0));
        assert_eq!(last.0, 600.0);
        assert_eq!(last.1, 0.0);
    }
}
