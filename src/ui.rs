//! Terminal UI helpers (colors + prompts).

use anyhow::{Context, Result};
use std::io::{self, IsTerminal, Write};

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

pub fn prompt_yes_no(prompt: &str, default_yes: bool) -> Result<bool> {
    let default_hint = if default_yes { "Y/n" } else { "y/N" };
    loop {
        print!(
            "{} {} [{}] ",
            label(Stream::Stdout, "?", "35"),
            prompt,
            default_hint
        );
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

        warn("Please answer with 'y' or 'n'.");
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
