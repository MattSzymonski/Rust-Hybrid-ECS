//! Native command-line entry point for the renderer asset cooker.
//!
//! # Responsibilities
//!
//! - Accepts source/output directories and an optional `--watch` flag.
//! - Reports rebuilt/cached outputs and retries failed cooks in watch mode.
//!
//! # Design
//!
//! The tool calls the same content-addressed pipeline for one-shot and watched
//! builds. Watching polls every 500 ms; failed cooks leave the last published
//! manifest available to the host. This binary is gated by `asset-cooking`.

// =============================================================================
// Command-Line Driver
// =============================================================================

/// Cook once, or poll indefinitely when the third argument is `--watch`.
///
/// # Errors
///
/// Missing directories in the command line and one-shot cooking failures are
/// returned to the process entry point. Watch mode logs failures and keeps polling.
fn main() -> anyhow::Result<()> {
    // Step 1: retain native path encoding while reading positional arguments.
    let mut args = std::env::args_os().skip(1);
    let root = std::path::PathBuf::from(
        args.next()
            .ok_or_else(|| anyhow::anyhow!("usage: pill-cook INPUT OUTPUT [--watch]"))?,
    );
    let output = std::path::PathBuf::from(
        args.next()
            .ok_or_else(|| anyhow::anyhow!("usage: pill-cook INPUT OUTPUT [--watch]"))?,
    );
    let watch = args.next().is_some_and(|a| a == "--watch");
    // Step 2: let the generation cache decide whether any inputs need rebuilding.
    loop {
        match pill_master_renderer::pill_assets::cook(&root, &output) {
            Ok(stats) => {
                if !stats.rebuilt.is_empty() || !watch {
                    println!(
                        "{} cooked, {} cached",
                        stats.rebuilt.len(),
                        stats.skipped.len()
                    );
                }
            }
            Err(e) => {
                if !watch {
                    return Err(e);
                }
                eprintln!("[assets] {e:#}");
            }
        }
        if !watch {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    Ok(())
}
