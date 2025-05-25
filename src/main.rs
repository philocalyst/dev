use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use dev::DevDocsManager;
use dirs;
use tokio::fs;
use webbrowser;

#[derive(Parser)]
#[clap(
    name = "devdocs",
    version = "1.0",
    author = "You <you@example.com>",
    about = "Manage DevDocs documentation locally"
)]
struct Cli {
    #[clap(subcommand)]
    cmd: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Add one or more docs (downloads & caches them)
    Add {
        /// Only generate HTML files
        #[clap(long)]
        html: bool,
        /// Only generate Markdown files
        #[clap(long)]
        md: bool,
        /// Slugs of docs to install
        slugs: Vec<String>,
    },

    /// Remove one or more docs
    Remove {
        /// Only remove HTML files
        #[clap(long)]
        html: bool,
        /// Only remove Markdown files
        #[clap(long)]
        md: bool,
        /// Slugs of docs to remove
        slugs: Vec<String>,
    },

    /// Fuzzy‐search across installed docs
    Search {
        /// Query string
        query: String,
        /// Maximum number of results
        #[clap(short, long)]
        limit: Option<usize>,
        /// Show absolute paths instead of relative
        #[clap(long)]
        absolute: bool,
    },

    /// Update docs by slug, or use "all" to update everything
    Update {
        /// Slugs to update, or the single token "all"
        slugs: Vec<String>,
    },

    /// Preview a doc file (relative to cache or an absolute path)
    Preview {
        /// Path to the file to preview (.md → stdout, .html → browser)
        path: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // reconstruct the same data directory the library uses
    let data_dir = dirs::data_local_dir()
        .expect("couldn’t find local data dir")
        .join("devdocs");

    let mgr = DevDocsManager::new()?;
    mgr.init().await?;

    match cli.cmd {
        Commands::Add { html, md, slugs } => {
            // default to both if neither flag is set
            let do_html = html || (!html && !md);
            let do_md = md || (!html && !md);

            for slug in slugs {
                if !mgr.is_doc_installed(&slug).await? {
                    // install the binary cache + markdown
                    mgr.add_doc(&slug).await?;
                    println!("✅ installed `{}` (markdown)", slug);

                    if do_html {
                        // TODO: expose an API in the library to generate HTML
                        eprintln!("⚠ html‐only install isn’t yet supported by the library");
                    }
                } else {
                    println!("⚠ `{}` already installed, skipping", slug);
                }
            }
        }

        Commands::Remove { html, md, slugs } => {
            let do_html = html || (!html && !md);
            let do_md = md || (!html && !md);

            for slug in slugs {
                if !mgr.is_doc_installed(&slug).await? {
                    eprintln!("⚠ `{}` is not installed", slug);
                    continue;
                }
                // remove from cache
                mgr.remove_doc(&slug).await?;
                println!("🗑 removed cache for `{}`", slug);

                let doc_dir = data_dir.join(&slug);
                if do_md && doc_dir.exists() {
                    // remove all .md under that dir
                    let _ = fs::remove_dir_all(&doc_dir).await;
                    println!("🗑 removed markdown files for `{}`", slug);
                }
                if do_html {
                    // TODO: same for html files once supported
                    eprintln!("⚠ html‐only removal isn’t yet supported by the library");
                }
            }
        }

        Commands::Search {
            query,
            limit,
            absolute,
        } => {
            let results = mgr.search(&query, limit).await?;
            for r in results {
                let rel = PathBuf::from(&r.entry.doc_slug).join(&r.entry.entry.path);
                let display_path = if absolute { data_dir.join(&rel) } else { rel };
                println!("{}\t{}", display_path.display(), r.entry.entry.name);
            }
        }

        Commands::Update { slugs } => {
            if slugs.len() == 1 && slugs[0] == "all" {
                println!("🔄 updating all installed docs…");
                mgr.update_all().await?;
            } else {
                for slug in slugs {
                    print!("🔄 updating `{}` … ", slug);
                    if let Err(e) = mgr.update_doc(&slug).await {
                        eprintln!("failed: {}", e);
                    } else {
                        println!("ok");
                    }
                }
            }
        }

        Commands::Preview { path } => {
            // resolve to absolute
            let mut file = PathBuf::from(&path);
            if !file.is_absolute() {
                file = data_dir.join(file);
            }
            if !file.exists() {
                anyhow::bail!("file not found: {}", file.display());
            }
            match file.extension().and_then(|s| s.to_str()) {
                Some("html") => {
                    webbrowser::open(&file.to_string_lossy())?;
                }
                _ => {
                    // default to printing markdown
                    let txt = fs::read_to_string(&file).await?;
                    print!("{txt}");
                }
            }
        }
    }

    Ok(())
}
