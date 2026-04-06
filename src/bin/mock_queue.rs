use anyhow::Result;
use chrono::{Datelike, Local, Timelike};
use clap::Parser;
use std::io::{self, Write};
use std::thread;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Parser, Debug)]
#[command(about = "Emit queue-shaped log lines for testing vs-queue.")]
struct Cli {
    #[arg(default_value_t = 90)]
    start_position: u32,
    #[arg(long, default_value_t = 750)]
    interval_ms: u64,
    #[arg(long, default_value_t = 3)]
    stall_every: u32,
}

struct SimpleRng {
    state: u64,
}

impl SimpleRng {
    fn seeded() -> Self {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15);
        Self { state: seed.max(1) }
    }

    fn next_u32(&mut self) -> u32 {
        let mut value = self.state;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.state = value;
        value as u32
    }

    fn chance(&mut self, numerator: u32, denominator: u32) -> bool {
        denominator > 0 && self.next_u32() % denominator < numerator
    }

    fn range_inclusive(&mut self, start: u32, end: u32) -> u32 {
        if start >= end {
            return start;
        }

        let width = end - start + 1;
        start + (self.next_u32() % width)
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut position = cli.start_position;
    let mut tick = 0_u32;
    let mut rng = SimpleRng::seeded();
    let stall_period = cli.stall_every.max(2);
    let jitter_span = (cli.interval_ms / 2).max(25);

    while position > 0 {
        tick += 1;
        if let Err(error) = emit_queue_line(position) {
            if error.kind() == io::ErrorKind::BrokenPipe {
                return Ok(());
            }
            return Err(error.into());
        }

        let jitter = rng.range_inclusive(0, (jitter_span as u32).saturating_mul(2)) as i64
            - jitter_span as i64;
        let delay_ms = (cli.interval_ms as i64 + jitter).max(40) as u64;
        thread::sleep(Duration::from_millis(delay_ms));

        let should_stall = cli.stall_every > 0
            && (tick % stall_period == 0 || rng.chance(1, stall_period.saturating_add(2)));
        if should_stall {
            continue;
        }

        let max_step = if position > 40 {
            5
        } else if position > 15 {
            4
        } else {
            3
        };
        let mut step = rng.range_inclusive(1, max_step.min(position));
        if rng.chance(1, 6) {
            step = step.saturating_add(1).min(position);
        }
        position = position.saturating_sub(step);
    }

    if let Err(error) = emit_queue_line(0) {
        if error.kind() != io::ErrorKind::BrokenPipe {
            return Err(error.into());
        }
    }

    Ok(())
}

fn emit_queue_line(position: u32) -> io::Result<()> {
    let now = Local::now();
    let mut stdout = io::stdout();
    writeln!(
        stdout,
        "{}.{}.{} {:02}:{:02}:{:02} [Client Notification] Client is in connect queue at position: {}",
        now.day(),
        now.month(),
        now.year(),
        now.hour(),
        now.minute(),
        now.second(),
        position
    )?;
    stdout.flush()
}
