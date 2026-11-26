use std::io::{self, Stdout, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::style::{
    Attribute, Color, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::terminal::{
    self, BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use crossterm::{execute, queue};

use node_modules_cleaner::{
    FoundDir, delete_dir, format_size, scan_node_modules_with_progress,
};

struct Args {
    root: PathBuf,
    list_only: bool,
    delete_all: bool,
    min_mb: u64,
}

fn print_help() {
    println!(
        "\
node-modules-cleaner - Fast CLI to scan and delete node_modules folders

USAGE:
    node-modules-cleaner [OPTIONS]

OPTIONS:
    --path, -p <DIR>     Root directory to scan (default: current directory '.')
    --list, -l           Print matching folders without entering interactive mode
    --delete --yes       Delete all matching folders without confirmation
    --min-size <MB>      Only include folders >= N MB (default: 0)
    --help, -h           Show this help message

INTERACTIVE CONTROLS:
    Up / Down (or k/j)   Move selection
    Enter (twice)        Select and confirm deletion of node_modules
    q / Esc              Quit

Created with ❤️ by @paaiz"
    );
}

fn parse_args() -> Result<Args, String> {
    let mut root = PathBuf::from(".");
    let mut list_only = false;
    let mut delete_flag = false;
    let mut yes_flag = false;
    let mut min_mb = 0u64;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--path" | "-p" => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("Flag {arg} requires a directory path"))?;
                root = PathBuf::from(value);
            }
            "--list" | "-l" | "--dry-run" => list_only = true,
            "--delete" | "-d" => delete_flag = true,
            "--yes" | "-y" => yes_flag = true,
            "--min-size" => {
                let value = args
                    .next()
                    .ok_or_else(|| "Flag --min-size requires a number (MB)".to_string())?;
                min_mb = value
                    .parse::<u64>()
                    .map_err(|_| format!("Invalid --min-size value: {value}"))?;
            }
            "--help" | "-h" => {
                print_help();
                process::exit(0);
            }
            other => {
                return Err(format!(
                    "Unknown argument: {other}\nRun with --help for usage."
                ));
            }
        }
    }

    Ok(Args {
        root,
        list_only,
        delete_all: delete_flag && yes_flag,
        min_mb,
    })
}

/// RAII guard so raw mode and alternate screen are always restored on exit.
struct TerminalGuard;

impl TerminalGuard {
    fn enter(stdout: &mut Stdout) -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        execute!(stdout, EnterAlternateScreen, Hide)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = execute!(stdout, Show, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

struct TuiSummary {
    deleted_count: usize,
    freed_bytes: u64,
}

fn draw_ui(
    stdout: &mut Stdout,
    root: &Path,
    items: &[FoundDir],
    cursor: usize,
    scroll: usize,
    status: &str,
    is_confirming: bool,
    freed_bytes: u64,
    deleted_count: usize,
) -> io::Result<()> {
    let (width, height) = terminal::size().unwrap_or((100, 24));
    let max_width = width.saturating_sub(2) as usize;
    let header_lines = 3usize;
    let footer_lines = 2usize;
    let list_height = (height as usize)
        .saturating_sub(header_lines + footer_lines)
        .max(1);

    let total_remaining: u64 = items.iter().map(|i| i.size_bytes).sum();

    // Synchronized update + MoveTo(0, 0) + line-by-line clear prevents flickering.
    queue!(stdout, BeginSynchronizedUpdate, MoveTo(0, 0))?;

    // Header line 1
    queue!(
        stdout,
        SetAttribute(Attribute::Bold),
        SetForegroundColor(Color::Cyan)
    )?;
    write!(stdout, " node-modules-cleaner ")?;
    queue!(stdout, SetAttribute(Attribute::Reset), ResetColor)?;
    write!(
        stdout,
        "| Remaining: {} ({}) | Deleted: {} ({})",
        items.len(),
        format_size(total_remaining),
        deleted_count,
        format_size(freed_bytes)
    )?;
    queue!(stdout, Clear(ClearType::UntilNewLine))?;
    write!(stdout, "\r\n")?;

    // Header line 2
    queue!(stdout, SetForegroundColor(Color::DarkGrey))?;
    write!(
        stdout,
        " Controls: [Up/Down] Select  [Enter] Delete (with confirm)  [q / Esc] Quit"
    )?;
    queue!(stdout, Clear(ClearType::UntilNewLine))?;
    write!(stdout, "\r\n")?;

    // Header line 3 (divider)
    write!(stdout, "{}", "-".repeat(max_width.min(100)))?;
    queue!(stdout, ResetColor, Clear(ClearType::UntilNewLine))?;
    write!(stdout, "\r\n")?;

    // List rows
    if items.is_empty() {
        queue!(stdout, SetForegroundColor(Color::Green))?;
        write!(stdout, " All listed node_modules folders have been deleted!")?;
        queue!(stdout, ResetColor, Clear(ClearType::UntilNewLine))?;
        write!(stdout, "\r\n")?;
    } else {
        let end = (scroll + list_height).min(items.len());
        let size_col_width = 15usize; // "  [1234.56 MB]"
        let path_col_width = max_width.min(96).saturating_sub(size_col_width).max(10);

        for (idx, item) in items.iter().enumerate().take(end).skip(scroll) {
            let is_selected = idx == cursor;
            let project = item.relative_project_path(root);
            let size = format_size(item.size_bytes);

            let project_display = if project.chars().count() > path_col_width {
                let head: String = project
                    .chars()
                    .take(path_col_width.saturating_sub(3))
                    .collect();
                format!("{head}...")
            } else {
                project
            };
            let line = format!(" {project_display:<path_col_width$}  [{size:>10}]");

            if is_selected {
                queue!(
                    stdout,
                    SetAttribute(Attribute::Bold),
                    SetForegroundColor(Color::Black),
                    SetBackgroundColor(Color::Cyan)
                )?;
                write!(stdout, ">{line}")?;
                queue!(stdout, SetAttribute(Attribute::Reset), ResetColor)?;
            } else {
                write!(stdout, " {line}")?;
            }
            queue!(stdout, Clear(ClearType::UntilNewLine))?;
            write!(stdout, "\r\n")?;
        }
    }

    queue!(stdout, Clear(ClearType::FromCursorDown))?;

    let footer_row = height.saturating_sub(2);
    queue!(stdout, MoveTo(0, footer_row))?;
    if is_confirming {
        queue!(
            stdout,
            SetAttribute(Attribute::Bold),
            SetForegroundColor(Color::Red)
        )?;
    } else {
        queue!(stdout, SetForegroundColor(Color::Yellow))?;
    }
    let status_truncated: String = status.chars().take(max_width).collect();
    write!(stdout, " {status_truncated}")?;
    queue!(
        stdout,
        SetAttribute(Attribute::Reset),
        ResetColor,
        Clear(ClearType::UntilNewLine)
    )?;

    let credit_row = height.saturating_sub(1);
    queue!(
        stdout,
        MoveTo(0, credit_row),
        SetForegroundColor(Color::DarkGrey)
    )?;
    write!(stdout, " Created with ❤️ by @paaiz")?;
    queue!(
        stdout,
        ResetColor,
        Clear(ClearType::UntilNewLine),
        EndSynchronizedUpdate
    )?;

    stdout.flush()
}

fn run_interactive(root: &Path, mut items: Vec<FoundDir>) -> io::Result<TuiSummary> {
    let mut stdout = io::stdout();
    let _guard = TerminalGuard::enter(&mut stdout)?;

    let mut cursor = 0usize;
    let mut scroll = 0usize;
    let mut freed_bytes = 0u64;
    let mut deleted_count = 0usize;
    let mut confirming: Option<usize> = None;
    let mut status = String::from("Select a folder and press Enter to delete.");
    let mut needs_redraw = true;

    loop {
        if needs_redraw {
            let (_, height) = terminal::size().unwrap_or((100, 24));
            let list_height = (height as usize).saturating_sub(5).max(1);

            if cursor >= items.len() && !items.is_empty() {
                cursor = items.len() - 1;
            }
            if cursor < scroll {
                scroll = cursor;
            } else if cursor >= scroll + list_height {
                scroll = cursor + 1 - list_height;
            }

            draw_ui(
                &mut stdout,
                root,
                &items,
                cursor,
                scroll,
                &status,
                confirming.is_some(),
                freed_bytes,
                deleted_count,
            )?;
            needs_redraw = false;
        }

        match event::read()? {
            Event::Resize(_, _) => {
                needs_redraw = true;
            }
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c')
                {
                    break;
                }

                if let Some(target_idx) = confirming {
                    match key.code {
                        KeyCode::Enter => {
                            confirming = None;
                            let target = items[target_idx].clone();
                            let project = target.project_name();
                            status = format!("Deleting {project}/node_modules ...");
                            draw_ui(
                                &mut stdout,
                                root,
                                &items,
                                cursor,
                                scroll,
                                &status,
                                false,
                                freed_bytes,
                                deleted_count,
                            )?;

                            match delete_dir(&target) {
                                Ok(freed) => {
                                    freed_bytes = freed_bytes.saturating_add(freed);
                                    deleted_count += 1;
                                    items.remove(target_idx);
                                    status = format!(
                                        "Deleted: {project}/node_modules ({})",
                                        format_size(freed)
                                    );
                                }
                                Err(err) => {
                                    status = format!("Failed to delete {project}: {err}");
                                }
                            }
                        }
                        _ => {
                            confirming = None;
                            status = String::from("Deletion cancelled.");
                        }
                    }
                    needs_redraw = true;
                    continue;
                }

                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Up | KeyCode::Char('k') => {
                        if cursor > 0 {
                            cursor -= 1;
                            needs_redraw = true;
                        }
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if !items.is_empty() && cursor + 1 < items.len() {
                            cursor += 1;
                            needs_redraw = true;
                        }
                    }
                    KeyCode::Enter => {
                        if items.is_empty() {
                            continue;
                        }
                        let target = &items[cursor];
                        confirming = Some(cursor);
                        status = format!(
                            "Confirm delete \"{}/node_modules\" ({})? Press [Enter] again to delete, [Esc] to cancel",
                            target.project_name(),
                            format_size(target.size_bytes)
                        );
                        needs_redraw = true;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    Ok(TuiSummary {
        deleted_count,
        freed_bytes,
    })
}

fn delete_all_non_interactive(items: &[FoundDir]) {
    let mut freed_total = 0u64;
    let mut deleted_count = 0usize;

    for target in items {
        print!("Deleting {} ... ", target.path.display());
        let _ = io::stdout().flush();

        match delete_dir(target) {
            Ok(freed) => {
                println!("OK ({})", format_size(freed));
                freed_total = freed_total.saturating_add(freed);
                deleted_count += 1;
            }
            Err(err) => {
                println!("FAILED ({err})");
            }
        }
    }

    println!(
        "\nDone: {deleted_count} folder(s) deleted, total space freed: {}",
        format_size(freed_total)
    );
}

fn scan_with_live_status(root: &PathBuf, min_bytes: u64) -> io::Result<Vec<FoundDir>> {
    let found_counter = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicBool::new(false));

    let counter_clone = Arc::clone(&found_counter);
    let done_clone = Arc::clone(&done);
    let root_label = root.display().to_string();

    let spinner_handle = thread::spawn(move || {
        let frames = ['|', '/', '-', '\\'];
        let mut idx = 0usize;
        while !done_clone.load(Ordering::Relaxed) {
            let count = counter_clone.load(Ordering::Relaxed);
            print!(
                "\r{} Scanning {} ... ({} node_modules found)   ",
                frames[idx % frames.len()],
                root_label,
                count
            );
            let _ = io::stdout().flush();
            idx += 1;
            thread::sleep(Duration::from_millis(100));
        }
        print!("\r{}\r", " ".repeat(80));
        let _ = io::stdout().flush();
    });

    let result = scan_node_modules_with_progress(root, min_bytes, Some(&found_counter));
    done.store(true, Ordering::Relaxed);
    let _ = spinner_handle.join();
    result
}

#[cfg(windows)]
fn launched_by_double_click() -> bool {
    unsafe extern "system" {
        fn GetConsoleProcessList(process_list: *mut u32, count: u32) -> u32;
    }
    let mut list = [0u32; 2];
    // Count == 1 means Windows created a dedicated console just for this .exe.
    unsafe { GetConsoleProcessList(list.as_mut_ptr(), 2) == 1 }
}

#[cfg(not(windows))]
fn launched_by_double_click() -> bool {
    false
}

fn wait_for_enter_if_double_clicked(double_clicked: bool) {
    if !double_clicked {
        return;
    }
    print!("\nPress Enter to exit...");
    let _ = io::stdout().flush();
    let mut buf = String::new();
    let _ = io::stdin().read_line(&mut buf);
}

fn prompt_for_path() -> Option<PathBuf> {
    print!("Enter a folder path to scan (or press Enter to exit): ");
    let _ = io::stdout().flush();
    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() {
        return None;
    }
    let trimmed = input.trim().trim_matches('"');
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

fn main() {
    let double_clicked = launched_by_double_click();

    let mut args = match parse_args() {
        Ok(a) => a,
        Err(err) => {
            eprintln!("Error: {err}");
            wait_for_enter_if_double_clicked(double_clicked);
            process::exit(1);
        }
    };

    let min_bytes = args.min_mb.saturating_mul(1024 * 1024);

    let found = loop {
        let start = Instant::now();
        let items = match scan_with_live_status(&args.root, min_bytes) {
            Ok(items) => items,
            Err(err) => {
                eprintln!("Scan failed: {err}");
                if double_clicked {
                    if let Some(next_root) = prompt_for_path() {
                        args.root = next_root;
                        continue;
                    }
                }
                wait_for_enter_if_double_clicked(double_clicked);
                process::exit(1);
            }
        };

        let elapsed = start.elapsed();
        if !items.is_empty() {
            break items;
        }

        println!(
            "No node_modules folders found in \"{}\" (finished in {:.2?}).",
            args.root.display(),
            elapsed
        );
        if double_clicked && !args.list_only && !args.delete_all {
            if let Some(next_root) = prompt_for_path() {
                args.root = next_root;
                continue;
            }
        }
        return;
    };

    if args.list_only {
        let total_bytes: u64 = found.iter().map(|f| f.size_bytes).sum();
        println!(
            "Found {} node_modules folder(s) (total {}):\n",
            found.len(),
            format_size(total_bytes),
        );
        for (i, item) in found.iter().enumerate() {
            println!(
                "  {:>3}. {:<24} [{:>10}]  {}",
                i + 1,
                item.project_name(),
                format_size(item.size_bytes),
                item.path.display()
            );
        }
        wait_for_enter_if_double_clicked(double_clicked);
        return;
    }

    if args.delete_all {
        delete_all_non_interactive(&found);
        wait_for_enter_if_double_clicked(double_clicked);
        return;
    }

    match run_interactive(&args.root, found) {
        Ok(summary) => {
            println!(
                "Done: {} node_modules folder(s) deleted, total space freed: {}.",
                summary.deleted_count,
                format_size(summary.freed_bytes)
            );
        }
        Err(err) => {
            eprintln!("Failed to run interactive mode: {err}");
            wait_for_enter_if_double_clicked(double_clicked);
            process::exit(1);
        }
    }

    wait_for_enter_if_double_clicked(double_clicked);
}
