# vs-queue - Vintage Story server queue helper

`vs-queue` tracks queue progress from a log file containing lines like:

```text
6.4.2026 17:07:03 [Client Notification] Client is in connect queue at position: 90
```

It keeps a rolling estimate of when the queue will reach zero and draws the observed progress plus the projection in a live terminal UI built with [`ratatui`](https://ratatui.rs/).

When you launch a process through `vs-queue`, the child is detached from the tracker on Unix and writes its output to a real log file. In the TUI, `q`, `Esc`, or `Ctrl-C` stop only `vs-queue`; the launched process keeps running. When the child reaches queue position `0` and exits, the TUI stays open on the final chart until you quit it manually.

## Build

```bash
cargo build --release
```

## Usage

Launch a process and track its log file:

```bash
./target/release/vs-queue /path/to/executable -- child-arg-1 child-arg-2
```

Launch with an explicit log file path:

```bash
./target/release/vs-queue --log-file ./queue.log /path/to/executable -- child-arg-1 child-arg-2
```

Watch an existing log file without launching anything:

```bash
./target/release/vs-queue --watch ./queue.log
```

Notes:

- The child process path can be absolute, relative, or a command name on `PATH`.
- Put `--` before the child arguments so `vs-queue` stops parsing its own flags.
- `stdout` and `stderr` from the child are both redirected into the same log file.
- The child is launched with `stdin` closed, so this mode is intended for non-interactive processes.
- In an interactive terminal, `vs-queue` uses an alternate-screen TUI instead of printing full-screen updates repeatedly.

## Quick Test

Build both binaries:

```bash
cargo build --bins
```

Run the randomized mock queue producer through the tracker:

```bash
./target/debug/vs-queue ./target/debug/mock_queue -- 90 --interval-ms 500
```

The command will create a log file like `./vs-queue-mock_queue-YYYYMMDD-HHMMSS.log` and follow it.

You can also rerun the tracker against that file later:

```bash
./target/debug/vs-queue --watch ./vs-queue-mock_queue-YYYYMMDD-HHMMSS.log
```

## Options

```text
--watch <LOG_FILE>         Watch an existing log file instead of launching
--log-file <LOG_FILE>      Log file path used when launching
--regression-samples <N>  Number of queue samples used in the rolling ETA fit
--recent-logs <N>         Number of recent log lines kept in the live UI
--refresh-ms <N>          UI refresh interval in milliseconds
```
