mod cli;
mod config;
mod core;
mod shell;

use std::fs;
use anyhow::Result;
use clap::Parser;
use cli::{Cli, Command};
use config::Config;

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::load()?;
    let cli = Cli::parse();

    match cli.command {
        Command::Run => {
            shell::run::run(config).await?;
        }
        Command::Report { format, chart } => {
            let jsonl_path = config.data_dir.join("events.jsonl");
            let contents = fs::read_to_string(&jsonl_path)
                .map_err(|e| anyhow::anyhow!("cannot read {:?}: {e}", jsonl_path))?;

            let lines: Vec<serde_json::Value> = contents
                .lines()
                .filter(|l| !l.trim().is_empty())
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect();

            let records: Vec<core::chain::Record> = lines
                .iter()
                .filter_map(|v| serde_json::from_value::<core::chain::Record>(v.clone()).ok())
                .collect();

            use core::dossier::*;
            let charter = ServiceCharter {
                repair_window_days: 0,
            };
            let tariff = Tariff {
                complete_per_day: 100.0,
                irregular_per_day: 50.0,
            };
            let cfg = DossierConfig {
                daytime_start: 8,
                daytime_end: 20,
                temp_threshold: 35.0,
            };

            let dossier = project(&lines, &charter, &tariff, &cfg);

            match format.as_str() {
                "md" => {
                    let report_cfg = core::render_md::ReportConfig { chart_path: chart };
                    let md = core::render_md::render_markdown(&dossier, &records, &report_cfg);
                    println!("{}", md);
                }
                "pdf" => {
                    let chart_path = chart.as_ref().map(std::path::Path::new);
                    let output = config.data_dir.join("dossier.pdf");
                    shell::render_pdf::render_pdf(&dossier, &records, chart_path, &output)
                        .map_err(|e| anyhow::anyhow!("{}", e))?;
                    println!("PDF written to {:?}", output);
                }
                other => {
                    anyhow::bail!("unsupported format: {other} (expected 'md' or 'pdf')");
                }
            }
        }
    }

    Ok(())
}
