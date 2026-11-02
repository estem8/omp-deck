//! `omp-deck-host`: per-session host daemon (`serve`) and debugging/CI commands.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::process::ExitCode;

use alacritty_terminal::Term;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::Config;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::vte::ansi::Processor;
use deck_host::daemon::{ServeOptions, serve};
use deck_host::{Namespace, SessionSpec};

const USAGE: &str = "\
usage: omp-deck-host <command> [--ns <namespace>] ...
  serve --name <name> --spec <file> [--foreground]   run a session daemon
  new <name> [--cwd <dir>] [--env K=V]... [--size <cols>x<rows>] [--tsp] -- <program> [args...]
                                                     start a detached session (--tsp: the host
                                                     acts as a TSP terminal for the program)
  ls                                                 list live sessions
  info <name>                                        session details (JSON)
  kill <name>                                        kill a session and its process tree
  dump <name> [--history <rows>]                     print a session's screen as text
  tsp <name> [--follow]                              print a TSP session's document as an outline
--ns defaults to omp-deck.";

struct Args {
    command: String,
    ns: String,
    positional: Vec<String>,
    name: Option<String>,
    spec: Option<PathBuf>,
    foreground: bool,
    cwd: Option<PathBuf>,
    env: Vec<(String, String)>,
    size: (u16, u16),
    history: u32,
    program: Vec<String>,
    tsp: bool,
    follow: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let command = it.next().ok_or_else(|| USAGE.to_owned())?;
    let mut args = Args {
        command,
        ns: "omp-deck".to_owned(),
        positional: Vec::new(),
        name: None,
        spec: None,
        foreground: false,
        cwd: None,
        env: Vec::new(),
        size: (80, 24),
        history: 0,
        program: Vec::new(),
        tsp: false,
        follow: false,
    };
    let value = |it: &mut std::iter::Skip<std::env::Args>, flag: &str| {
        it.next().ok_or_else(|| format!("{flag} needs a value"))
    };
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--ns" => args.ns = value(&mut it, "--ns")?,
            "--name" => args.name = Some(value(&mut it, "--name")?),
            "--spec" => args.spec = Some(value(&mut it, "--spec")?.into()),
            "--foreground" => args.foreground = true,
            "--tsp" => args.tsp = true,
            "--follow" => args.follow = true,
            "--cwd" => args.cwd = Some(value(&mut it, "--cwd")?.into()),
            "--env" => {
                let kv = value(&mut it, "--env")?;
                let (k, v) = kv
                    .split_once('=')
                    .ok_or_else(|| format!("--env {kv}: expected K=V"))?;
                args.env.push((k.to_owned(), v.to_owned()));
            }
            "--size" => {
                let s = value(&mut it, "--size")?;
                args.size = s
                    .split_once('x')
                    .and_then(|(c, r)| Some((c.parse().ok()?, r.parse().ok()?)))
                    .filter(|&(c, r): &(u16, u16)| c > 0 && r > 0)
                    .ok_or_else(|| format!("--size {s}: expected <cols>x<rows>"))?;
            }
            "--history" => {
                let s = value(&mut it, "--history")?;
                args.history = s
                    .parse()
                    .map_err(|_| format!("--history {s}: expected a number"))?;
            }
            "--" => {
                args.program = it.by_ref().collect();
                break;
            }
            "-h" | "--help" => return Err(USAGE.to_owned()),
            flag if flag.starts_with("--") => {
                return Err(format!("unknown option {flag}\n{USAGE}"));
            }
            _ => args.positional.push(arg),
        }
    }
    Ok(args)
}

fn one_name(args: &Args) -> Result<&str, String> {
    match args.positional.as_slice() {
        [name] => Ok(name),
        _ => Err(format!(
            "{} needs exactly one session name\n{USAGE}",
            args.command
        )),
    }
}

fn run(args: &Args) -> Result<(), String> {
    let err = |e: deck_host::Error| e.to_string();
    if args.command == "serve" {
        let name = args.name.as_deref().ok_or("serve needs --name")?;
        let spec_file = args.spec.as_deref().ok_or("serve needs --spec")?;
        let opts = ServeOptions {
            ns: &args.ns,
            name,
            spec_file,
            foreground: args.foreground,
        };
        return serve(&opts).map_err(err);
    }
    let ns = Namespace::new(&args.ns).map_err(err)?;
    match args.command.as_str() {
        "ls" => {
            for name in ns.list() {
                println!("{name}");
            }
            Ok(())
        }
        "info" => {
            let info = ns.info(one_name(args)?).map_err(err)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&info).map_err(|e| e.to_string())?
            );
            Ok(())
        }
        "kill" => ns.kill(one_name(args)?).map_err(err),
        "dump" => {
            let (info, snapshot) =
                deck_host::peek(&ns, one_name(args)?, args.history).map_err(err)?;
            print!(
                "{}",
                render_text(&snapshot, info.cols, info.rows, args.history as usize)
            );
            Ok(())
        }
        "tsp" => {
            let name = one_name(args)?;
            let mut last = String::new();
            loop {
                let outline = render_outline(&deck_host::tsp_peek(&ns, name).map_err(err)?);
                if outline != last {
                    if args.follow && !last.is_empty() {
                        println!("--");
                    }
                    print!("{outline}");
                    last = outline;
                }
                if !args.follow {
                    return Ok(());
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
        "new" => {
            let name = one_name(args)?;
            let (program, rest) = args
                .program
                .split_first()
                .ok_or("new needs -- <program> [args...]")?;
            let cwd = match &args.cwd {
                Some(dir) => dir.clone(),
                None => std::env::current_dir().map_err(|e| format!("current directory: {e}"))?,
            };
            let mut env_set = vec![
                ("TERM".to_owned(), "xterm-256color".to_owned()),
                ("COLORTERM".to_owned(), "truecolor".to_owned()),
            ];
            for (k, v) in &args.env {
                env_set.retain(|(key, _)| key != k);
                env_set.push((k.clone(), v.clone()));
            }
            let spec = SessionSpec {
                name: name.to_owned(),
                program: program.into(),
                args: rest.to_vec(),
                cwd,
                env_set,
                env_remove: Vec::new(),
                tsp: args.tsp,
            };
            deck_host::start(&ns, &spec, args.size.0, args.size.1).map_err(err)
        }
        other => Err(format!("unknown command {other}\n{USAGE}")),
    }
}

/// Replays a snapshot into a terminal of the session's size and returns its text (scrollback
/// then screen; trailing blanks and blank trailing lines trimmed).
fn render_text(snapshot: &[u8], cols: u16, rows: u16, history: usize) -> String {
    let config = Config {
        scrolling_history: history,
        ..Config::default()
    };
    let size = TermSize::new(usize::from(cols.max(2)), usize::from(rows.max(1)));
    let mut term = Term::new(config, &size, VoidListener);
    Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new().advance(&mut term, snapshot);
    let grid = term.grid();
    let mut lines = Vec::new();
    for line in grid.topmost_line().0..grid.screen_lines() as i32 {
        let row = &grid[Line(line)];
        let mut text = String::new();
        for col in 0..grid.columns() {
            let cell = &row[Column(col)];
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            text.push(cell.c);
            text.extend(cell.zerowidth().unwrap_or(&[]));
        }
        lines.push(text.trim_end().to_owned());
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// Longest text shown per node in the outline.
const OUTLINE_TEXT: usize = 80;

/// A TSP document (`deck_host::tsp_peek`) as an indented outline: one block per surface, one
/// line per node with its kind, id, role, sendable flag and the start of its text.
fn render_outline(document: &serde_json::Value) -> String {
    use std::fmt::Write as _;

    fn node(out: &mut String, n: &serde_json::Value, depth: usize) {
        let kind = n["k"].as_str().unwrap_or("?");
        let id = n["id"].as_str().unwrap_or("?");
        let _ = write!(out, "{:indent$}{kind} #{id}", "", indent = depth * 2);
        let p = &n["p"];
        if let Some(role) = p["role"].as_str() {
            let _ = write!(out, " role={role}");
        }
        if let Some(sendable) = p["sendable"].as_bool() {
            let _ = write!(out, " sendable={sendable}");
        }
        // `text` for markdown/plain nodes, concatenated `spans` for styled `text` nodes.
        let text = match p["text"].as_str() {
            Some(t) => t.to_owned(),
            None => p["spans"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|s| s["t"].as_str())
                .collect(),
        };
        if !text.is_empty() {
            let mut shown: String = text.chars().take(OUTLINE_TEXT).collect();
            if text.chars().nth(OUTLINE_TEXT).is_some() {
                shown.push('…');
            }
            let _ = write!(out, " {shown:?}");
        }
        out.push('\n');
        for child in n["c"].as_array().into_iter().flatten() {
            node(out, child, depth + 1);
        }
    }

    let mut out = String::new();
    for s in document["surfaces"].as_array().into_iter().flatten() {
        let _ = write!(
            out,
            "surface {} {}",
            s["id"].as_str().unwrap_or("?"),
            s["mode"].as_str().unwrap_or("?")
        );
        if let Some(role) = s["role"].as_str() {
            let _ = write!(out, " role={role}");
        }
        let _ = write!(
            out,
            " {} seq={} nodes={}",
            if s["open"].as_bool() == Some(true) {
                "open"
            } else {
                "closed"
            },
            s["seq"],
            s["nodes"]
        );
        if let Some(focus) = s["focus"].as_str() {
            let _ = write!(out, " focus=#{focus}");
        }
        out.push('\n');
        node(&mut out, &s["root"], 1);
    }
    out
}

/// Release builds use the GUI subsystem: attach to the parent console so CLI output shows
/// (unless stdout is already redirected).
#[cfg(all(windows, not(debug_assertions)))]
fn attach_parent_console() {
    use windows_sys::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_OUTPUT_HANDLE,
    };
    // SAFETY: plain Win32 calls without pointer arguments.
    unsafe {
        let out = GetStdHandle(STD_OUTPUT_HANDLE);
        if out.is_null() {
            AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };
    #[cfg(all(windows, not(debug_assertions)))]
    if args.command != "serve" {
        attach_parent_console();
    }
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("omp-deck-host {}: {msg}", args.command);
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::render_outline;

    #[test]
    fn outline_lists_surfaces_nodes_and_truncates_text() {
        let long = "x".repeat(100);
        let doc = json!({"surfaces": [{
            "id": "s1", "mode": "inline", "role": "omp.session", "open": true, "seq": 7,
            "focus": "ed", "nodes": 4,
            "root": {"id": "s1", "k": "col", "c": [
                {"id": "main", "k": "col", "c": [
                    {"id": "m1", "k": "md", "p": {"text": long}},
                    {"id": "m2", "k": "md", "p": {"text": "multi\nline", "role": "assistant"}},
                ]},
                {"id": "dock", "k": "col", "c": [
                    {"id": "ed", "k": "editor", "p": {"sendable": true, "text": ""}},
                    {"id": "st", "k": "text", "p": {"spans": [{"t": "no-", "s": "a"}, {"t": "model"}]}},
                ]},
            ]}}], "blobs": []});
        let expected = format!(
            "surface s1 inline role=omp.session open seq=7 nodes=4 focus=#ed\n\
             \x20 col #s1\n\
             \x20   col #main\n\
             \x20     md #m1 \"{}…\"\n\
             \x20     md #m2 role=assistant \"multi\\nline\"\n\
             \x20   col #dock\n\
             \x20     editor #ed sendable=true\n\
             \x20     text #st \"no-model\"\n",
            "x".repeat(80)
        );
        assert_eq!(render_outline(&doc), expected);
        assert_eq!(render_outline(&json!({"surfaces": [], "blobs": []})), "");
    }
}
