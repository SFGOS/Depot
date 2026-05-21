//! Terminal UI helpers (colors + prompts).

use anyhow::{Context, Result};
use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};

static ASSUME_YES: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy)]
enum Stream {
    Stdout,
    Stderr,
}

fn colors_enabled(stream: Stream) -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if matches!(std::env::var("TERM").as_deref(), Ok("dumb")) {
        return false;
    }

    match stream {
        Stream::Stdout => io::stdout().is_terminal(),
        Stream::Stderr => io::stderr().is_terminal(),
    }
}

fn paint(stream: Stream, text: &str, color_code: &str) -> String {
    if colors_enabled(stream) {
        format!("\x1b[{}m{}\x1b[0m", color_code, text)
    } else {
        text.to_string()
    }
}

fn label(stream: Stream, text: &str, color_code: &str) -> String {
    format!("[{}]", paint(stream, text, color_code))
}

pub fn info(message: impl AsRef<str>) {
    println!(
        "{} {}",
        label(Stream::Stdout, "INFO", "36"),
        message.as_ref()
    );
}

pub fn success(message: impl AsRef<str>) {
    println!("{} {}", label(Stream::Stdout, "OK", "32"), message.as_ref());
}

pub fn merge_package(layer: &str, package: &str) {
    println!(
        "{} {} {} into layer {}",
        paint(Stream::Stdout, ">>>", "32;1"),
        paint(Stream::Stdout, "merging package", "36;1"),
        paint(Stream::Stdout, package, "32;1"),
        layer
    );
}

pub fn warn(message: impl AsRef<str>) {
    eprintln!(
        "{} {}",
        label(Stream::Stderr, "WARN", "33"),
        message.as_ref()
    );
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        $crate::ui::info(format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_ok {
    ($($arg:tt)*) => {
        $crate::ui::success(format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
        $crate::ui::warn(format!($($arg)*))
    };
}

fn parse_yes_no_input(input: &str, default_yes: bool) -> Option<bool> {
    let trimmed = input.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        return Some(default_yes);
    }
    match trimmed.as_str() {
        "y" | "yes" => Some(true),
        "n" | "no" => Some(false),
        _ => None,
    }
}

/// Configure global prompt behavior for the current process.
pub fn set_assume_yes(assume_yes: bool) {
    ASSUME_YES.store(assume_yes, Ordering::Relaxed);
}

pub fn assume_yes_enabled() -> bool {
    ASSUME_YES.load(Ordering::Relaxed)
}

pub fn prompt_yes_no(prompt: &str, default_yes: bool) -> Result<bool> {
    if ASSUME_YES.load(Ordering::Relaxed) {
        info(format!("{} [auto-yes]", prompt));
        return Ok(true);
    }

    let default_hint = if default_yes { "Y/n" } else { "y/N" };
    loop {
        print!("{prompt} [{default_hint}]: ");
        io::stdout()
            .flush()
            .context("Failed to flush prompt to stdout")?;

        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .context("Failed to read user input from stdin")?;

        if let Some(answer) = parse_yes_no_input(&input, default_yes) {
            return Ok(answer);
        }

        warn("Invalid choice. Please answer with 'y' or 'n'.");
    }
}

/// Prompt for a package-oriented action with a Starpack-like layout.
pub fn prompt_package_action(action: &str, packages: &[String], default_yes: bool) -> Result<bool> {
    if packages.is_empty() {
        return prompt_yes_no(
            &format!("No packages were selected for {action}. Continue?"),
            default_yes,
        );
    }

    println!();
    println!("The following packages will be processed for {}:", action);
    println!("  {}", packages.join(" "));
    prompt_yes_no("Proceed?", default_yes)
}

/// Prompt the user to choose one option by index.
pub fn prompt_select_index(prompt: &str, options: &[String], default_idx: usize) -> Result<usize> {
    if options.is_empty() {
        anyhow::bail!("No options available for selection");
    }
    let default_idx = default_idx.min(options.len() - 1);
    if ASSUME_YES.load(Ordering::Relaxed) {
        info(format!(
            "{} [auto-select {}: {}]",
            prompt,
            default_idx + 1,
            options[default_idx]
        ));
        return Ok(default_idx);
    }

    loop {
        println!("{}:", prompt);
        for (idx, option) in options.iter().enumerate() {
            println!(
                "  {}) {}{}",
                idx + 1,
                option,
                if idx == default_idx { " [default]" } else { "" }
            );
        }
        print!(
            "Choose option [1-{}] (Enter = {}): ",
            options.len(),
            default_idx + 1
        );
        io::stdout()
            .flush()
            .context("Failed to flush prompt to stdout")?;

        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .context("Failed to read user input from stdin")?;
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Ok(default_idx);
        }
        if let Ok(num) = trimmed.parse::<usize>()
            && (1..=options.len()).contains(&num)
        {
            return Ok(num - 1);
        }
        warn(format!(
            "Invalid choice. Please enter a number between 1 and {}.",
            options.len()
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::parse_yes_no_input;

    #[test]
    fn parse_yes_no_defaults() {
        assert_eq!(parse_yes_no_input("", true), Some(true));
        assert_eq!(parse_yes_no_input("", false), Some(false));
    }

    #[test]
    fn parse_yes_no_values() {
        assert_eq!(parse_yes_no_input("y", false), Some(true));
        assert_eq!(parse_yes_no_input("yes", false), Some(true));
        assert_eq!(parse_yes_no_input("N", true), Some(false));
        assert_eq!(parse_yes_no_input("no", true), Some(false));
        assert_eq!(parse_yes_no_input("maybe", true), None);
    }
}
