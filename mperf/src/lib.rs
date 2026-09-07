mod counter_selection;
mod disassembly;
mod doctor;
mod event_dispatcher;
mod events_export;
mod postprocess;
mod query;
mod record;
mod roofline;
mod source;
mod stat;
mod tui;
mod utils;

use std::{
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::Result;
use clap::{Parser, Subcommand};

use doctor::do_doctor;
use events_export::do_events_export;
use mperf_data::Scenario;
use query::{QueryFormat, do_query};
use record::do_record;
use stat::do_stat;

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    List,
    /// Diagnose this host's profiling readiness.
    Doctor,
    Stat {
        #[arg(short, long)]
        pid: Option<u32>,
        /// Comma-separated event names (use `mperf list` to discover them).
        #[arg(short = 'e', long = "event", value_delimiter = ',')]
        events: Vec<String>,
        /// Render the host Top-down methodology instead of the flat stat table.
        #[arg(long)]
        topdown: bool,
        /// Maximum Top-down tree level to display (default: 1).
        #[arg(short = 'l', long, default_value_t = 1)]
        level: u8,
        #[arg(last = true)]
        command: Vec<String>,
    },
    Record {
        #[arg(short, long)]
        scenario: Scenario,
        /// Roofline accounting implementation. The default probes the workload and host.
        #[arg(long, value_enum, default_value_t = roofline::BackendKind::Auto)]
        roofline_backend: roofline::BackendKind,
        /// QEMU user-mode binary; auto-detected from the guest ELF when omitted.
        #[arg(long)]
        qemu: Option<PathBuf>,
        /// QEMU plugin shared library; defaults to the artifact next to mperf.
        #[arg(long)]
        qemu_plugin: Option<PathBuf>,
        /// Extra argument passed to QEMU before the guest executable.
        #[arg(long, allow_hyphen_values = true)]
        qemu_arg: Vec<String>,
        /// DynamoRIO drrun launcher or build/bundle directory; defaults to the bundle next to mperf or PATH.
        #[arg(long)]
        dynamorio: Option<PathBuf>,
        /// miniperf DynamoRIO client shared library; normally discovered automatically.
        #[arg(long)]
        dynamorio_client: Option<PathBuf>,
        #[arg(short, long)]
        output_directory: String,
        #[arg(short, long)]
        pid: Option<u32>,
        /// Stop a snapshot after this duration (for example `10s` or `2m`).
        #[arg(long, value_parser = parse_duration)]
        duration: Option<std::time::Duration>,
        #[arg(last = true)]
        command: Vec<String>,
    },
    Show {
        result_directory: String,
    },
    EventsExport {
        result_directory: String,
    },
    /// Validate a session directory after a crash, quarantining segments
    /// that were left without a footer.
    Recover {
        result_directory: String,
    },
    /// Run one read-only SQL query against recorded performance data.
    Query {
        /// Result directory followed by one quoted SQL statement. Use
        /// `mperf query help` for the complete guide.
        #[arg(value_names = ["RESULT_DIRECTORY", "SQL"], num_args = 0..=2)]
        arguments: Vec<String>,
        /// Output format.
        #[arg(short, long, value_enum, default_value_t = QueryFormat::Text)]
        format: QueryFormat,
        /// Read SQL from a file instead of the command line. Use `-` for stdin.
        #[arg(long, value_name = "PATH")]
        file: Option<String>,
        /// Maximum rows emitted, independent of any SQL LIMIT.
        #[arg(long, default_value_t = 50, value_parser = query::parse_max_rows)]
        max_rows: usize,
    },
}

fn parse_duration(value: &str) -> std::result::Result<std::time::Duration, String> {
    let (number, multiplier) = if let Some(value) = value.strip_suffix("ms") {
        (value, 0.001)
    } else if let Some(value) = value.strip_suffix('s') {
        (value, 1.0)
    } else if let Some(value) = value.strip_suffix('m') {
        (value, 60.0)
    } else if let Some(value) = value.strip_suffix('h') {
        (value, 3600.0)
    } else {
        (value, 1.0)
    };
    let number = number
        .parse::<f64>()
        .map_err(|_| format!("invalid duration '{value}'"))?;
    if !number.is_finite() || number <= 0.0 {
        return Err("duration must be a positive finite value".to_string());
    }
    Ok(std::time::Duration::from_secs_f64(number * multiplier))
}

/// Parse the command line and run the miniperf application.
pub async fn run() -> Result<()> {
    let args = Cli::parse();

    match args.command {
        Commands::Stat {
            pid,
            events,
            topdown,
            level,
            command,
        } => do_stat(pid, command, events, topdown.then_some(level)),
        Commands::Doctor => do_doctor(),
        Commands::List => {
            let events = libprof::list_supported_counters(libprof::DriverKind::Default);
            for event in events {
                println!("{} - {}", event.name(), event.description());
            }
            Ok(())
        }
        Commands::Record {
            scenario,
            roofline_backend,
            qemu,
            qemu_plugin,
            qemu_arg,
            dynamorio,
            dynamorio_client,
            output_directory,
            pid,
            duration,
            command,
        } => {
            let roofline = roofline::Options {
                backend: roofline_backend,
                qemu,
                qemu_plugin,
                qemu_args: qemu_arg,
                dynamorio,
                dynamorio_client,
            };
            roofline.validate_for(scenario)?;

            if std::fs::exists(&output_directory)? {
                return Err(Into::<anyhow::Error>::into(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!("'{output_directory}' already exists"),
                ))
                .context("profiling results must be put in different directories"));
            }
            std::fs::create_dir_all(&output_directory)?;
            let output_directory = PathBuf::from_str(&output_directory)?;
            let recorded = do_record(
                scenario,
                &output_directory,
                pid,
                command,
                roofline,
                duration,
            )
            .await;
            // Without info.json there is no recording here, only whatever
            // segments the dispatcher had opened when the run failed. Keeping
            // that directory hands the viewer files it cannot open and makes
            // the obvious retry, the same `-o` path, refuse to run.
            if recorded.is_err() && !output_directory.join("info.json").exists() {
                let _ = std::fs::remove_dir_all(&output_directory);
            }
            recorded
        }
        Commands::Show { result_directory } => tui::tui_main(Path::new(&result_directory)).await,
        Commands::Recover { result_directory } => {
            let report = store::recover_session(Path::new(&result_directory))?;
            println!("{} healthy segment(s)", report.healthy);
            for file in &report.quarantined {
                println!("quarantined {}", file.display());
            }
            if report.quarantined.is_empty() {
                println!("no crash-damaged segments found");
            }
            Ok(())
        }
        Commands::EventsExport { result_directory } => {
            do_events_export(Path::new(&result_directory));
            Ok(())
        }
        Commands::Query {
            arguments,
            format,
            file,
            max_rows,
        } => do_query(arguments, file, format, max_rows),
    }
}
