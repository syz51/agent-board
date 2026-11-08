use clap::Parser as _;

fn main() {
    let cli = board::cli::Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("BOARD_LOG")
                // The daemon and its hook relay also say what they do (starts, adoptions,
                // reconnects, hooks that passed the relay by) in watch.log; everything else
                // speaks only of trouble.
                .unwrap_or_else(|_| {
                    tracing_subscriber::EnvFilter::new("warn,board::daemon=info,board::relay=info")
                }),
        )
        .with_writer(std::io::stderr)
        .init();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("starting the async runtime");
    let code = runtime.block_on(board::cli::run(cli));
    // Exit without waiting for stray blocking tasks (a hook must not outlive its budget).
    std::process::exit(code);
}
