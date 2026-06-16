use std::time::Duration;

use env_logger::TimestampPrecision;
use human_panic::setup_panic;

use dezoomify_rs::{Arguments, ZoomError, dezoomify_with_cancel, process_bulk_with_cancel};
use log::{error, info, warn};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() {
    setup_panic!();
    let has_args = std::env::args_os().count() > 1;
    let mut has_errors = false;
    let args: Arguments = clap::Parser::parse();
    init_log(&args);

    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            info!("Cancellation requested. Shutting down gracefully...");
            cancel_clone.cancel();
            // If graceful shutdown is blocked on synchronous I/O (e.g. stdin or an
            // in-flight network request), give it a short grace period and then exit.
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(1)) => {
                    warn!("Shutdown timeout reached, forcing exit.");
                    std::process::exit(130);
                }
                _ = tokio::signal::ctrl_c() => {
                    warn!("Second interrupt received, forcing exit.");
                    std::process::exit(130);
                }
            }
        }
    });

    if args.is_bulk_mode() {
        // Bulk processing mode
        match process_bulk_with_cancel(&args, cancel.clone()).await {
            Ok(stats) => {
                if stats.failed_images > 0 || stats.partial_downloads > 0 {
                    has_errors = true;
                }
            }
            Err(ZoomError::Cancelled) => {
                warn!("Cancelled by user.");
                std::process::exit(130);
            }
            Err(err) => {
                error!("{err}");
                has_errors = true;
            }
        }
    } else {
        // Single processing mode (existing behavior)
        loop {
            if cancel.is_cancelled() {
                warn!("Cancelled by user.");
                std::process::exit(130);
            }
            match dezoomify_with_cancel(&args, cancel.clone()).await {
                Ok(saved_as) => {
                    info!(
                        "Image successfully saved to '{}'",
                        saved_as.to_string_lossy(),
                    );
                }
                Err(ZoomError::Io { source })
                    if source.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    // If we have reached the end of stdin, we exit
                    warn!("Reached end of input. Exiting...");
                    break;
                }
                Err(ZoomError::Cancelled) => {
                    warn!("Cancelled by user.");
                    std::process::exit(130);
                }
                Err(err @ ZoomError::PartialDownload { .. }) => {
                    warn!("{err}");
                    has_errors = true;
                }
                Err(err) => {
                    error!("{err}");
                    has_errors = true;
                }
            }
            if has_args {
                // Command-line invocation
                break;
            }
        }
    }
    if has_errors {
        std::process::exit(1);
    }
}

fn init_log(args: &Arguments) {
    let logging = &args.logging;
    let is_default_logging = logging.eq_ignore_ascii_case("info");
    let env = env_logger::Env::new().default_filter_or(logging);
    env_logger::Builder::from_env(env)
        .format_timestamp(if is_default_logging {
            None
        } else {
            Some(TimestampPrecision::Millis)
        })
        .format_target(!is_default_logging)
        .init();
}
