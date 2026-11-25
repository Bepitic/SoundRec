use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SizedSample};

use crossterm::{
    QueueableCommand,
    cursor::{Hide, MoveTo, Show},
    event::{self, Event, KeyCode, KeyModifiers},
    style::{Color, ResetColor, SetForegroundColor},
    terminal::{
        self, Clear, ClearType, DisableLineWrap, EnableLineWrap, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};

use std::{
    error::Error,
    fs,
    io::{self, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

const BAR: char = '▉';
const GAIN: f32 = 2.6;
const MIN_HEIGHT: usize = 9;
const REFRESH_MS: u64 = 50;

fn capture<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    tx: mpsc::Sender<Vec<f32>>,
    running: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
) -> Result<cpal::Stream, Box<dyn Error>>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = config.channels as usize;

    Ok(device.build_input_stream(
        config.clone(),
        move |data: &[T], _| {
            let samples = data
                .chunks_exact(channels)
                .map(|frame| {
                    frame.iter().map(|&s| f32::from_sample(s)).sum::<f32>() / channels as f32
                })
                .collect();

            let _ = tx.send(samples);
        },
        move |_| {
            failed.store(true, Ordering::Relaxed);
            running.store(false, Ordering::Relaxed);
        },
        None,
    )?)
}

// Align text without overlapping.
fn aligned(width: usize, left: &str, center: &str, right: &str) -> String {
    let mut line = vec![' '; width];

    let left: Vec<char> = left.chars().take(width).collect();
    line[..left.len()].copy_from_slice(&left);

    let right: Vec<char> = right.chars().collect();

    let right_start = if left.len() + right.len() + 1 <= width {
        let start = width - right.len();
        line[start..].copy_from_slice(&right);
        start
    } else {
        width
    };

    let center: Vec<char> = center.chars().collect();

    if center.len() <= width {
        let start = (width - center.len()) / 2;
        let end = start + center.len();

        if start > left.len() && end < right_start {
            line[start..end].copy_from_slice(&center);
        }
    }

    line.into_iter().collect()
}

fn draw(samples: &[f32], elapsed: u64, filename: &str, rate: u32) -> io::Result<()> {
    let mut out = io::stdout().lock();

    let (cols, rows) = terminal::size()?;

    if cols == 0 || rows == 0 {
        return Ok(());
    }

    let width = cols as usize;
    let rows = rows as usize;

    // Calculate audio level.
    let rms = if samples.is_empty() {
        0.0
    } else {
        (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
    };

    let db = 20.0 * rms.max(0.000001).log10();

    let level = ((db + 60.0) / 60.0).clamp(0.0, 1.0);

    let timer = format!(
        "REC {:02}:{:02}:{:02}",
        elapsed / 3600,
        elapsed / 60 % 60,
        elapsed % 60
    );

    let db_label = format!("{db:.1} dBFS");

    let short_filename = filename.rsplit('/').next().unwrap_or(filename);

    // Header.
    let header = aligned(width, &timer, short_filename, &db_label);

    out.queue(MoveTo(0, 0))?;
    out.queue(SetForegroundColor(Color::DarkGrey))?;
    write!(out, "{header}")?;

    // Red recording indicator.
    out.queue(MoveTo(0, 0))?;
    out.queue(SetForegroundColor(Color::Red))?;

    write!(out, "{}", "REC".chars().take(width).collect::<String>())?;

    // ----------------------------------------
    // Compact horizontal meter
    // ----------------------------------------

    if rows < MIN_HEIGHT + 2 {
        // Clear remaining rows.
        for y in 1..rows {
            out.queue(MoveTo(0, y as u16))?;
            write!(out, "{}", " ".repeat(width))?;
        }

        let filled = (level * width as f32).round() as usize;

        if rows >= 2 {
            let meter_row = if rows >= 3 { (rows - 1) / 2 } else { 1 };

            out.queue(MoveTo(0, meter_row as u16))?;

            out.queue(SetForegroundColor(Color::Green))?;
            write!(out, "{}", "█".repeat(filled))?;

            out.queue(SetForegroundColor(Color::DarkGrey))?;
            write!(out, "{}", "░".repeat(width - filled))?;
        } else if width >= 30 {
            // Single-line mode: timer + meter + dB.
            let available = width.saturating_sub(timer.len() + db_label.len() + 2);

            let filled = (level * available as f32).round() as usize;

            out.queue(MoveTo((timer.len() + 1) as u16, 0))?;

            out.queue(SetForegroundColor(Color::Green))?;
            write!(out, "{}", "█".repeat(filled))?;

            out.queue(SetForegroundColor(Color::DarkGrey))?;
            write!(out, "{}", "░".repeat(available - filled))?;
        }

        if rows >= 3 {
            let footer = aligned(
                width,
                &format!("WAV / {rate} Hz / Mono"),
                "",
                "q / Ctrl+C to save",
            );

            out.queue(MoveTo(0, (rows - 1) as u16))?;
            out.queue(SetForegroundColor(Color::DarkGrey))?;

            write!(out, "{footer}")?;
        }

        out.queue(ResetColor)?;

        return out.flush();
    }

    // ----------------------------------------
    // Full waveform
    // ----------------------------------------

    let separators = rows >= MIN_HEIGHT + 4;

    let top = if separators { 2 } else { 1 };
    let bottom = if separators { 2 } else { 1 };

    let height = rows.saturating_sub(top + bottom);

    // Upper separator.
    if separators {
        out.queue(MoveTo(0, 1))?;
        out.queue(SetForegroundColor(Color::DarkGrey))?;

        write!(out, "{}", "─".repeat(width))?;
    }

    // Calculate the peak amplitude of each column.
    let peaks: Vec<f32> = (0..width)
        .map(|x| {
            if samples.is_empty() {
                return 0.0;
            }

            let start = x * samples.len() / width;

            let end = ((x + 1) * samples.len() / width)
                .max(start + 1)
                .min(samples.len());

            samples[start..end]
                .iter()
                .map(|s| s.abs())
                .fold(0.0_f32, f32::max)
        })
        .collect();

    // Draw symmetrical vertical bars.
    out.queue(SetForegroundColor(Color::Green))?;

    if height > 0 {
        let center = (height as f32 - 1.0) / 2.0;

        for y in 0..height {
            let distance = (y as f32 - center).abs();

            let line: String = peaks
                .iter()
                .map(|&peak| {
                    let amplitude = (peak * GAIN).clamp(0.0, 1.0);

                    let bar_height = amplitude * center;

                    if distance <= bar_height.max(0.5) {
                        BAR
                    } else {
                        ' '
                    }
                })
                .collect();

            out.queue(MoveTo(0, (top + y) as u16))?;

            write!(out, "{line}")?;
        }
    }

    // Lower separator.
    if separators {
        out.queue(MoveTo(0, (rows - 2) as u16))?;
        out.queue(SetForegroundColor(Color::DarkGrey))?;

        write!(out, "{}", "─".repeat(width))?;
    }

    // Footer.
    let footer = aligned(
        width,
        &format!("WAV / {rate} Hz / Mono"),
        "",
        "q / Ctrl+C to save",
    );

    out.queue(MoveTo(0, (rows - 1) as u16))?;
    out.queue(SetForegroundColor(Color::DarkGrey))?;

    write!(out, "{footer}")?;

    out.queue(ResetColor)?;
    out.flush()
}

// --------------------------------------------
// Terminal management
// --------------------------------------------

struct TerminalGuard;

impl TerminalGuard {
    fn new() -> io::Result<Self> {
        let guard = Self;

        terminal::enable_raw_mode()?;

        let mut out = io::stdout();

        out.queue(EnterAlternateScreen)?;
        out.queue(DisableLineWrap)?;
        out.queue(Hide)?;
        out.queue(Clear(ClearType::All))?;

        out.flush()?;

        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut out = io::stdout();

        let _ = out.queue(ResetColor);
        let _ = out.queue(EnableLineWrap);
        let _ = out.queue(Show);
        let _ = out.queue(LeaveAlternateScreen);
        let _ = out.flush();

        let _ = terminal::disable_raw_mode();
    }
}

// --------------------------------------------
// Keyboard input
// --------------------------------------------

fn should_stop() -> io::Result<bool> {
    while event::poll(Duration::ZERO)? {
        if let Event::Key(key) = event::read()? {
            match key.code {
                KeyCode::Char('q') | KeyCode::Char('Q') => {
                    return Ok(true);
                }

                KeyCode::Char('c') | KeyCode::Char('C')
                    if key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    return Ok(true);
                }

                _ => {}
            }
        }
    }

    Ok(false)
}

// --------------------------------------------
// Main
// --------------------------------------------

fn main() -> Result<(), Box<dyn Error>> {
    fs::create_dir_all("recordings")?;

    let filename = format!(
        "recordings/{}.wav",
        chrono::Local::now().format("%Y-%m-%d_%H-%M-%S")
    );

    let host = cpal::default_host();

    let device = host.default_input_device().ok_or("No microphone found")?;

    let supported = device.default_input_config()?;
    let format = supported.sample_format();

    let config: cpal::StreamConfig = supported.into();
    let rate = config.sample_rate;

    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let mut writer = hound::WavWriter::create(&filename, spec)?;

    let running = Arc::new(AtomicBool::new(true));
    let failed = Arc::new(AtomicBool::new(false));
    let flag = running.clone();

    // Also handle external SIGINT.
    ctrlc::set_handler(move || {
        flag.store(false, Ordering::Relaxed);
    })?;

    let (tx, rx) = mpsc::channel::<Vec<f32>>();

    let stream = match format {
        cpal::SampleFormat::F32 => {
            capture::<f32>(&device, &config, tx, running.clone(), failed.clone())?
        }

        cpal::SampleFormat::I16 => {
            capture::<i16>(&device, &config, tx, running.clone(), failed.clone())?
        }

        cpal::SampleFormat::U16 => {
            capture::<u16>(&device, &config, tx, running.clone(), failed.clone())?
        }

        _ => return Err("Unsupported audio format".into()),
    };

    let terminal_guard = TerminalGuard::new()?;

    stream.play()?;

    let start = Instant::now();
    let mut last_draw = Instant::now();
    let mut latest_samples = Vec::new();

    draw(&[], 0, &filename, rate)?;

    // Recording loop.
    while running.load(Ordering::Relaxed) {
        if should_stop()? {
            break;
        }

        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(samples) => {
                for &sample in &samples {
                    writer.write_sample((sample.clamp(-1.0, 1.0) * 32767.0) as i16)?;
                }

                latest_samples = samples;
            }

            Err(mpsc::RecvTimeoutError::Timeout) => {}

            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break;
            }
        }

        if last_draw.elapsed() >= Duration::from_millis(REFRESH_MS) {
            draw(&latest_samples, start.elapsed().as_secs(), &filename, rate)?;

            last_draw = Instant::now();
        }
    }

    // Stop audio capture.
    drop(stream);

    // Save all remaining buffered audio.
    for samples in rx.try_iter() {
        for sample in samples {
            writer.write_sample((sample.clamp(-1.0, 1.0) * 32767.0) as i16)?;
        }
    }

    writer.finalize()?;

    // Restore terminal before printing.
    drop(terminal_guard);

    println!("Recording saved: {filename}");

    if failed.load(Ordering::Relaxed) {
        return Err("Recording stopped due to an audio error".into());
    }

    Ok(())
}
