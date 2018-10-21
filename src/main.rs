// `error_chain!` can recurse deeply
#![recursion_limit = "1024"]

#[macro_use]
extern crate error_chain;

#[macro_use]
extern crate clap;

#[macro_use]
extern crate lazy_static;

extern crate ansi_term;
extern crate atty;
extern crate console;
extern crate content_inspector;
extern crate directories;
extern crate encoding;
extern crate git2;
extern crate shell_words;
extern crate syntect;
extern crate wild;

mod app;
mod assets;
mod clap_app;
mod config;
mod controller;
mod decorations;
mod diff;
mod dirs;
mod inputfile;
mod line_range;
mod output;
mod preprocessor;
mod printer;
mod style;
mod syntax_mapping;
mod terminal;
mod util;

use std::collections::HashSet;
use std::io;
use std::io::Write;
use std::path::Path;
use std::process;

use ansi_term::Colour::Green;
use ansi_term::Style;

use app::{App, Config};
use assets::{clear_assets, config_dir, HighlightingAssets};
use config::config_file;
use controller::Controller;
use inputfile::InputFile;
use style::{OutputComponent, OutputComponents};

mod errors {
    error_chain! {
        foreign_links {
            Clap(::clap::Error);
            Io(::std::io::Error);
            SyntectError(::syntect::LoadingError);
            ParseIntError(::std::num::ParseIntError);
        }
    }

    pub fn handle_error(error: &Error) {
        match error {
            &Error(ErrorKind::Io(ref io_error), _)
                if io_error.kind() == super::io::ErrorKind::BrokenPipe =>
            {
                super::process::exit(0);
            }
            _ => {
                use ansi_term::Colour::Red;
                eprintln!("{}: {}", Red.paint("[bat error]"), error);
            }
        };
    }
}

use errors::*;

fn run_diff_subcommand(config: &Config) -> Option<()> {
    use git2::{DiffOptions, Repository};
    use std::path::Path;
    use line_range::{LineRange, LineRanges};

    let repo = Repository::discover(".").ok()?;

    let mut diff_options = DiffOptions::new();
    diff_options.context_lines(2);

    let diff = repo
        .diff_index_to_workdir(None, Some(&mut diff_options))
        .ok()?;

    let _ = diff.foreach(
        &mut |_, _| true,
        None,
        Some(&mut |delta, hunk| {
            let path = delta.new_file().path().unwrap_or_else(|| Path::new(""));

            let new_start = hunk.new_start();
            let new_lines = hunk.new_lines();
            let new_end = (new_start + new_lines) as i32 - 1;
            // println!("{:?} {}:{}", path, new_start, new_end);

            let path_str = path.to_string_lossy();
            let mut new_config = config.clone();
            new_config.files = vec![InputFile::Ordinary(&path_str)];
            new_config.line_ranges = LineRanges::from(vec![LineRange::from(&format!("{}:{}", new_start, new_end)).unwrap()]);
            run_controller(&new_config).unwrap(); // TODO

            true
        }),
        None
    );

    Some(())
}

fn run_cache_subcommand(matches: &clap::ArgMatches) -> Result<()> {
    if matches.is_present("init") {
        let source_dir = matches.value_of("source").map(Path::new);
        let target_dir = matches.value_of("target").map(Path::new);

        let blank = matches.is_present("blank");

        let assets = HighlightingAssets::from_files(source_dir, blank)?;
        assets.save(target_dir)?;
    } else if matches.is_present("clear") {
        clear_assets();
    } else if matches.is_present("config-dir") {
        writeln!(io::stdout(), "{}", config_dir())?;
    }

    Ok(())
}

pub fn list_languages(config: &Config) -> Result<()> {
    let assets = HighlightingAssets::new();
    let mut languages = assets
        .syntax_set
        .syntaxes()
        .iter()
        .filter(|syntax| !syntax.hidden && !syntax.file_extensions.is_empty())
        .collect::<Vec<_>>();
    languages.sort_by_key(|lang| lang.name.to_uppercase());

    let longest = languages
        .iter()
        .map(|syntax| syntax.name.len())
        .max()
        .unwrap_or(32); // Fallback width if they have no language definitions.

    let comma_separator = ", ";
    let separator = " ";
    // Line-wrapping for the possible file extension overflow.
    let desired_width = config.term_width - longest - separator.len();

    let stdout = io::stdout();
    let mut stdout = stdout.lock();

    let style = if config.colored_output {
        Green.normal()
    } else {
        Style::default()
    };

    for lang in languages {
        write!(stdout, "{:width$}{}", lang.name, separator, width = longest)?;

        // Number of characters on this line so far, wrap before `desired_width`
        let mut num_chars = 0;

        let mut extension = lang.file_extensions.iter().peekable();
        while let Some(word) = extension.next() {
            // If we can't fit this word in, then create a line break and align it in.
            let new_chars = word.len() + comma_separator.len();
            if num_chars + new_chars >= desired_width {
                num_chars = 0;
                write!(stdout, "\n{:width$}{}", "", separator, width = longest)?;
            }

            num_chars += new_chars;
            write!(stdout, "{}", style.paint(&word[..]))?;
            if extension.peek().is_some() {
                write!(stdout, "{}", comma_separator)?;
            }
        }
        writeln!(stdout)?;
    }

    Ok(())
}

pub fn list_themes(cfg: &Config) -> Result<()> {
    let assets = HighlightingAssets::new();
    let themes = &assets.theme_set.themes;
    let mut config = cfg.clone();
    let mut style = HashSet::new();
    style.insert(OutputComponent::Plain);
    config.files = vec![InputFile::ThemePreviewFile];
    config.output_components = OutputComponents(style);

    let stdout = io::stdout();
    let mut stdout = stdout.lock();

    if config.colored_output {
        for (theme, _) in themes.iter() {
            writeln!(
                stdout,
                "Theme: {}\n",
                Style::new().bold().paint(theme.to_string())
            )?;
            config.theme = theme.to_string();
            let _controller = Controller::new(&config, &assets).run();
            writeln!(stdout)?;
        }
    } else {
        for (theme, _) in themes.iter() {
            writeln!(stdout, "{}", theme)?;
        }
    }

    Ok(())
}

fn run_controller(config: &Config) -> Result<bool> {
    let assets = HighlightingAssets::new();
    let controller = Controller::new(&config, &assets);
    controller.run()
}

/// Returns `Err(..)` upon fatal errors. Otherwise, returns `Some(true)` on full success and
/// `Some(false)` if any intermediate errors occurred (were printed).
fn run() -> Result<bool> {
    let app = App::new()?;

    match app.matches.subcommand() {
        ("cache", Some(cache_matches)) => {
            // If there is a file named 'cache' in the current working directory,
            // arguments for subcommand 'cache' are not mandatory.
            // If there are non-zero arguments, execute the subcommand cache, else, open the file cache.
            if !cache_matches.args.is_empty() {
                run_cache_subcommand(cache_matches)?;
                Ok(true)
            } else {
                let mut config = app.config()?;
                config.files = vec![InputFile::Ordinary(&"cache")];

                run_controller(&config)
            }
        }
        ("diff", _) => {
            run_diff_subcommand(&app.config()?);
            Ok(true)
        }
        _ => {
            let config = app.config()?;

            if app.matches.is_present("list-languages") {
                list_languages(&config)?;

                Ok(true)
            } else if app.matches.is_present("list-themes") {
                list_themes(&config)?;

                Ok(true)
            } else if app.matches.is_present("config-file") {
                println!("{}", config_file().to_string_lossy());

                Ok(true)
            } else {
                run_controller(&config)
            }
        }
    }
}

fn main() {
    let result = run();

    match result {
        Err(error) => {
            handle_error(&error);
            process::exit(1);
        }
        Ok(false) => {
            process::exit(1);
        }
        Ok(true) => {
            process::exit(0);
        }
    }
}
