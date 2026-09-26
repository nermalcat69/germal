use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use germal_core::store::{Layout, Store, write_atomic};
use germal_recorder::{db::Db, germal, record, replay};

#[derive(Parser)]
#[command(about = "Record browser API traffic into SQLite, then load it back into Germal")]
struct Cli {
    /// SQLite database
    #[arg(long, global = true, default_value = "germal-recordings.db")]
    db: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Args)]
struct Filter {
    /// GET, POST or OTHER
    #[arg(long)]
    method: Option<String>,
    #[arg(long)]
    session: Option<i64>,
    #[arg(long)]
    host: Option<String>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Open Chromium and record xhr/fetch requests until Ctrl+C
    Record {
        /// Record every resource type, not just xhr/fetch
        #[arg(long)]
        all: bool,
        /// Open this page first
        #[arg(long)]
        url: Option<String>,
        /// Project to record into (created if missing)
        #[arg(long, default_value = "Default")]
        project: String,
    },
    /// List recorded requests
    List(Filter),
    /// Import recorded requests into Germal's saved requests (restart Germal to see them)
    Import(Filter),
    /// Compress (zstd) then encrypt (AES-256-GCM) the whole database into a .germal file
    Export { out: PathBuf },
    /// Decrypt a .germal file back into a SQLite database
    Unpack {
        file: PathBuf,
        /// Where to write the database (refuses to overwrite)
        out: PathBuf,
    },
    /// Load-test one recorded request
    Load {
        id: i64,
        #[arg(short = 'n', long, default_value_t = 100)]
        requests: u64,
        #[arg(short, long, default_value_t = 10)]
        concurrency: usize,
    },
}

/// 口令：GERMAL_PASSPHRASE 环境变量优先（脚本用），否则终端里不回显地读；加密时要输两遍。
fn passphrase(confirm: bool) -> Result<String> {
    if let Ok(p) = std::env::var("GERMAL_PASSPHRASE") {
        return Ok(p);
    }
    let p = rpassword::prompt_password("Passphrase: ")?;
    if confirm && p != rpassword::prompt_password("Repeat passphrase: ")? {
        bail!("passphrases do not match");
    }
    Ok(p)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        // chromiumoxide 认不出新版 Chrome 的部分 CDP 消息，逐条 warn 只是噪声（这些事件本来就不用）
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,chromiumoxide::handler=error".into()),
        )
        .init();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Record { all, url, project } => {
            println!(
                "Recording to {} — browse in the window; Ctrl+C or close the browser to stop",
                cli.db.display()
            );
            let project = Db::open(&cli.db)?.create_project(&project)?;
            let n = record::run(&cli.db, all, url.as_deref(), project, async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await?;
            println!("{n} requests saved");
        }
        Cmd::Export { out } => {
            let plain = Db::open(&cli.db)?.snapshot()?;
            let sealed = germal::seal(&plain, &passphrase(true)?)?;
            std::fs::write(&out, &sealed)?;
            println!(
                "{} → {} bytes ({} plain)",
                out.display(),
                sealed.len(),
                plain.len()
            );
        }
        Cmd::Unpack { file, out } => {
            let plain = germal::open(&std::fs::read(&file)?, &passphrase(false)?)?;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&out)?;
            std::io::Write::write_all(&mut f, &plain)?;
            println!("Wrote {}", out.display());
        }
        Cmd::List(f) => {
            let db = Db::open(&cli.db)?;
            for r in replay::select(&db, f.method.as_deref(), f.session, f.host.as_deref())? {
                let status = r.status.map_or("ERR".into(), |s| s.to_string());
                println!("{:>5}  {:<7} {:<3}  {}", r.id, r.method, status, r.url);
            }
        }
        Cmd::Import(f) => {
            let db = Db::open(&cli.db)?;
            let root = Store::default_root().context("no Germal data directory")?;
            let layout = Layout::new(root);
            layout.ensure()?;
            let (saved, skipped) =
                replay::take_new(&db, f.method.as_deref(), f.session, f.host.as_deref())?;
            let ok = saved.len();
            for s in saved {
                write_atomic(&layout.request_path(s.id), &serde_json::to_vec_pretty(&s)?)?;
            }
            println!("Imported {ok} new requests ({skipped} skipped: unsupported method)");
        }
        Cmd::Load {
            id,
            requests,
            concurrency,
        } => {
            let db = Db::open(&cli.db)?;
            let row = replay::select(&db, None, None, None)?
                .into_iter()
                .find(|r| r.id == id)
                .with_context(|| format!("no recorded request with id {id}"))?;
            let Some(draft) = replay::to_draft(&row) else {
                bail!("method {} is not supported", row.method)
            };
            let req = germal_core::http::prepare(&draft)?;
            println!("{} {}  ×{requests} @ {concurrency}", row.method, row.url);
            let r = germal_core::loadtest::run(
                &germal_core::http::build_client(),
                &req,
                requests,
                concurrency,
            )
            .await;
            println!(
                "done in {:.2?}: {:.1} req/s, {} completed, {} errors",
                r.elapsed, r.rps, r.completed, r.errors
            );
            println!("status: {:?}", r.statuses);
            println!(
                "latency  min {:?}  p50 {:?}  p90 {:?}  p99 {:?}  max {:?}",
                r.min, r.p50, r.p90, r.p99, r.max
            );
        }
    }
    Ok(())
}
