use comfy_table::{Attribute, Cell, Color, Table};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
struct LogEntry {
    r#type: String,
    payload: serde_json::Value,
}

#[derive(Deserialize)]
struct TokenCountPayload {
    r#type: String,
    info: Option<TokenUsageInfo>,
}

#[derive(Deserialize)]
struct TokenUsageInfo {
    total_token_usage: TokenUsage,
}

#[derive(Deserialize, Default, Clone)]
struct TokenUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cached_input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    reasoning_output_tokens: u64,
}

#[derive(Deserialize)]
struct SessionMeta {
    cwd: Option<String>,
}

#[derive(Clone, Copy)]
enum Period {
    Total,
    Week,
    Month,
}

impl Period {
    fn label(self) -> &'static str {
        match self {
            Period::Total => "all time",
            Period::Week => "past 7 days",
            Period::Month => "past 30 days",
        }
    }
}

/// GPT-5.4 API pricing per million tokens.
/// Cached input = $0.25/M, uncached input = $2.50/M, output = $15.00/M.
/// cached_input_tokens is a subset of input_tokens, so uncached = input - cached.
fn estimate_cost(stats: &ProjectStats) -> f64 {
    let m = 1_000_000.0;
    let uncached = stats.input_tokens.saturating_sub(stats.cached_input_tokens);
    uncached as f64 * 2.50 / m
        + stats.cached_input_tokens as f64 * 0.25 / m
        + stats.output_tokens as f64 * 15.00 / m
}

/// Returns (year, month, day) for today minus `days_ago` days.
fn date_minus_days(days_ago: u64) -> (u32, u32, u32) {
    use std::time::{SystemTime, UNIX_EPOCH};

    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let target_secs = now_secs - days_ago * 86400;

    // Algorithm from http://howardhinnant.github.io/date_algorithms.html
    let days = (target_secs / 86400) as i64;
    let z = days + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    (y as u32, m, d)
}

fn date_dir_in_range(year_str: &str, month_str: &str, day_str: &str, cutoff: (u32, u32, u32)) -> bool {
    let Ok(y) = year_str.parse::<u32>() else {
        return false;
    };
    let Ok(m) = month_str.parse::<u32>() else {
        return false;
    };
    let Ok(d) = day_str.parse::<u32>() else {
        return false;
    };
    (y, m, d) >= cutoff
}

#[derive(Default)]
struct ProjectStats {
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    reasoning_tokens: u64,
    sessions: u64,
}

impl ProjectStats {
    fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    fn accumulate(&mut self, other: &ProjectStats) {
        self.input_tokens += other.input_tokens;
        self.cached_input_tokens += other.cached_input_tokens;
        self.output_tokens += other.output_tokens;
        self.reasoning_tokens += other.reasoning_tokens;
        self.sessions += other.sessions;
    }
}

/// Normalize known broken cwd prefixes, then shorten for display.
fn normalize_cwd(path: &str) -> String {
    // Codex Desktop sometimes records /home/home/osso/ instead of /home/osso/
    let path = path.replace("/home/home/osso/", "/home/osso/");
    shorten_path(&path)
}

fn shorten_path(path: &str) -> String {
    for prefix in [
        "/syncthing/Sync/Projects/",
        "/home/osso/Projects/",
        "/home/osso/Repos/",
        "/home/osso/",
        "/syncthing/Sync/",
    ] {
        if let Some(rest) = path.strip_prefix(prefix) {
            let base = prefix
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or("");
            if ["Projects", "Repos"].contains(&base) {
                return rest.to_string();
            }
            return format!("{base}/{rest}");
        }
    }
    path.to_string()
}

/// Collect session files filtered by date directory structure.
/// Sessions live at `base/{YYYY}/{MM}/{DD}/*.jsonl`.
fn collect_session_files(base: &Path, cutoff: Option<(u32, u32, u32)>) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(years) = fs::read_dir(base) else {
        return files;
    };

    for year_entry in years.flatten() {
        if !year_entry.path().is_dir() {
            continue;
        }
        let year_name = year_entry.file_name().to_string_lossy().to_string();

        let Ok(months) = fs::read_dir(year_entry.path()) else {
            continue;
        };
        for month_entry in months.flatten() {
            if !month_entry.path().is_dir() {
                continue;
            }
            let month_name = month_entry.file_name().to_string_lossy().to_string();

            let Ok(days) = fs::read_dir(month_entry.path()) else {
                continue;
            };
            for day_entry in days.flatten() {
                if !day_entry.path().is_dir() {
                    continue;
                }
                let day_name = day_entry.file_name().to_string_lossy().to_string();

                if let Some(cutoff) = cutoff {
                    if !date_dir_in_range(&year_name, &month_name, &day_name, cutoff) {
                        continue;
                    }
                }

                let Ok(sessions) = fs::read_dir(day_entry.path()) else {
                    continue;
                };
                for session in sessions.flatten() {
                    let path = session.path();
                    if path.extension().is_some_and(|e| e == "jsonl") {
                        files.push(path);
                    }
                }
            }
        }
    }
    files
}

fn process_session(path: &Path) -> Option<(String, ProjectStats)> {
    let content = fs::read_to_string(path).ok()?;

    let mut cwd: Option<String> = None;
    let mut last_usage: Option<TokenUsage> = None;

    for line in content.lines() {
        let Ok(entry) = serde_json::from_str::<LogEntry>(line) else {
            continue;
        };

        match entry.r#type.as_str() {
            "session_meta" => {
                if let Ok(meta) = serde_json::from_value::<SessionMeta>(entry.payload) {
                    cwd = meta.cwd;
                }
            }
            "event_msg" => {
                if let Ok(tc) = serde_json::from_value::<TokenCountPayload>(entry.payload) {
                    if tc.r#type == "token_count" {
                        if let Some(info) = tc.info {
                            last_usage = Some(info.total_token_usage);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    let usage = last_usage?;
    let project = cwd
        .map(|c| normalize_cwd(&c))
        .unwrap_or_else(|| "unknown".to_string());

    let stats = ProjectStats {
        input_tokens: usage.input_tokens,
        cached_input_tokens: usage.cached_input_tokens,
        output_tokens: usage.output_tokens,
        reasoning_tokens: usage.reasoning_output_tokens,
        sessions: 1,
    };

    Some((project, stats))
}

fn format_tokens(n: u64) -> String {
    if n >= 1_000_000_000 {
        format!("{:.1}B", n as f64 / 1_000_000_000.0)
    } else if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn tokens_color(n: u64) -> Color {
    if n >= 100_000_000 {
        Color::Red
    } else if n >= 10_000_000 {
        Color::Yellow
    } else {
        Color::Green
    }
}

fn cost_color(cost: f64) -> Color {
    if cost > 100.0 {
        Color::Red
    } else if cost > 10.0 {
        Color::Yellow
    } else {
        Color::Green
    }
}

fn stats_row(rank: &str, name: &str, stats: &ProjectStats, bold_all: bool) -> Vec<Cell> {
    let attr = |c: Cell| {
        if bold_all {
            c.add_attribute(Attribute::Bold)
        } else {
            c
        }
    };
    let cost = estimate_cost(stats);
    vec![
        attr(Cell::new(rank)),
        attr(Cell::new(name)),
        attr(Cell::new(stats.sessions)),
        attr(Cell::new(format_tokens(stats.input_tokens))),
        attr(Cell::new(format_tokens(stats.cached_input_tokens))),
        attr(Cell::new(format_tokens(stats.output_tokens))),
        attr(Cell::new(format_tokens(stats.reasoning_tokens))),
        attr(Cell::new(format_tokens(stats.total_tokens()))
            .add_attribute(Attribute::Bold)
            .fg(tokens_color(stats.total_tokens()))),
        attr(Cell::new(format!("${:.2}", cost)).fg(cost_color(cost))),
    ]
}

fn print_leaderboard(sorted: &[(String, ProjectStats)], period: Period) {
    println!("Codex token usage ({}) — cost estimated at GPT-5.4 API rates\n", period.label());

    let mut table = Table::new();
    table.set_header(
        ["#", "Project", "Sessions", "Input", "Cached", "Output", "Reasoning", "Total", "Cost*"]
            .map(|h| Cell::new(h).add_attribute(Attribute::Bold)),
    );

    let mut grand_total = ProjectStats::default();
    for (i, (name, stats)) in sorted.iter().enumerate() {
        table.add_row(stats_row(&(i + 1).to_string(), name, stats, false));
        grand_total.accumulate(stats);
    }
    table.add_row(stats_row("", "TOTAL", &grand_total, true));

    println!("{table}");
    println!("\n* Cost is hypothetical (GPT-5.4: $2.50/M input, $0.25/M cached, $15/M output). Actual cost: $0 via ChatGPT Pro.");
}

/// Merge subdirectory entries into their closest parent project.
fn merge_subdirs(mut stats: HashMap<String, ProjectStats>) -> HashMap<String, ProjectStats> {
    let known: Vec<String> = stats.keys().cloned().collect();
    let mut merges: Vec<(String, String)> = Vec::new();

    for name in &known {
        let best_parent = known
            .iter()
            .filter(|p| *p != name && name.starts_with(&format!("{p}/")))
            .max_by_key(|p| p.len());

        if let Some(parent) = best_parent {
            if let Some(s) = stats.get(name) {
                if s.sessions <= 5 {
                    merges.push((name.clone(), parent.clone()));
                }
            }
        }
    }

    for (child, parent) in merges {
        if let Some(child_stats) = stats.remove(&child) {
            stats.entry(parent).or_default().accumulate(&child_stats);
        }
    }

    stats
}

fn gather_stats(period: Period) -> Vec<(String, ProjectStats)> {
    let sessions_dir = dirs::home_dir()
        .expect("no home dir")
        .join(".codex/sessions");

    if !sessions_dir.exists() {
        eprintln!("No Codex sessions found at {}", sessions_dir.display());
        std::process::exit(1);
    }

    let cutoff = match period {
        Period::Total => None,
        Period::Week => Some(date_minus_days(7)),
        Period::Month => Some(date_minus_days(30)),
    };

    let files = collect_session_files(&sessions_dir, cutoff);
    let mut all_stats: HashMap<String, ProjectStats> = HashMap::new();

    for path in &files {
        if let Some((project, stats)) = process_session(path) {
            all_stats.entry(project).or_default().accumulate(&stats);
        }
    }

    let all_stats = merge_subdirs(all_stats);

    let mut sorted: Vec<(String, ProjectStats)> = all_stats
        .into_iter()
        .filter(|(_, s)| s.sessions > 0)
        .collect();
    sorted.sort_by(|a, b| b.1.total_tokens().cmp(&a.1.total_tokens()));
    sorted
}

fn parse_period() -> Period {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("week" | "w" | "7d") => Period::Week,
        Some("month" | "m" | "30d") => Period::Month,
        Some("total" | "all" | "a") | None => Period::Total,
        Some(other) => {
            eprintln!("Unknown period: {other}");
            eprintln!("Usage: codex-tokens [week|month|total]");
            std::process::exit(1);
        }
    }
}

fn main() {
    let period = parse_period();
    let stats = gather_stats(period);
    print_leaderboard(&stats, period);
}
