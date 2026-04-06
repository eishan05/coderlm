use std::path::PathBuf;

use clap::Parser;
use tracing::info;

use coderlm_server::config;
use coderlm_server::mcp::server::CoderlmMcpServer;
use coderlm_server::server;
use coderlm_server::server::state::AppState;

#[derive(Parser)]
#[command(name = "coderlm", about = "CoderLM REPL server for code-aware agent sessions")]
struct Cli {
    /// Subcommand
    #[command(subcommand)]
    command: Commands,
}

#[derive(clap::Subcommand)]
enum Commands {
    /// Start the REPL server against a codebase
    Serve {
        /// Optional initial project directory to pre-index
        path: Option<PathBuf>,

        /// Port to listen on
        #[arg(short, long, default_value = "3000")]
        port: u16,

        /// Bind address
        #[arg(short, long, default_value = "127.0.0.1")]
        bind: String,

        /// Maximum file size in bytes to index
        #[arg(long, default_value_t = config::DEFAULT_MAX_FILE_SIZE)]
        max_file_size: u64,

        /// Maximum number of concurrent indexed projects
        #[arg(long, default_value = "5")]
        max_projects: usize,

        /// Start as an MCP (Model Context Protocol) server over stdio
        /// instead of the HTTP server. The project to index is taken
        /// from `path` or the current working directory.
        #[arg(long)]
        mcp: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Serve {
            path,
            port,
            bind,
            max_file_size,
            max_projects,
            mcp,
        } => {
            if mcp {
                run_mcp_server(path, max_file_size, max_projects).await?;
            } else {
                // Initialize tracing only for the HTTP server.
                // For MCP mode, stdout is the transport — tracing to stdout
                // would corrupt the JSON-RPC stream.
                tracing_subscriber::fmt()
                    .with_env_filter(
                        tracing_subscriber::EnvFilter::try_from_default_env()
                            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
                    )
                    .init();

                info!("coderlm v{}", env!("CARGO_PKG_VERSION"));
                run_server(path, port, bind, max_file_size, max_projects).await?;
            }
        }
    }

    Ok(())
}

async fn run_server(
    path: Option<PathBuf>,
    port: u16,
    bind: String,
    max_file_size: u64,
    max_projects: usize,
) -> anyhow::Result<()> {
    // Create shared state
    let state = AppState::new(max_projects, max_file_size);

    // If an initial path was provided, pre-index it
    if let Some(ref p) = path {
        info!("Pre-indexing project: {}", p.display());
        state.get_or_create_project(p).map_err(|e| {
            anyhow::anyhow!("Failed to index '{}': {}", p.display(), e)
        })?;
    }

    // Build router
    let app = server::build_router(state);

    let addr = format!("{}:{}", bind, port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    if let Some(ref p) = path {
        info!("coderlm serving {} on http://{}", p.display(), addr);
    } else {
        info!("coderlm server listening on http://{} (no project pre-indexed)", addr);
    }

    axum::serve(listener, app).await?;

    Ok(())
}

async fn run_mcp_server(
    path: Option<PathBuf>,
    max_file_size: u64,
    max_projects: usize,
) -> anyhow::Result<()> {
    // For MCP mode, initialise tracing to stderr so it doesn't corrupt
    // the JSON-RPC stream on stdout.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    info!("coderlm MCP server v{}", env!("CARGO_PKG_VERSION"));

    // Determine the project directory
    let cwd = match path {
        Some(p) => p,
        None => std::env::current_dir()?,
    };

    info!("MCP server indexing project: {}", cwd.display());

    let state = AppState::new(max_projects, max_file_size);
    let mcp_server = CoderlmMcpServer::new(state, &cwd)
        .map_err(|e| anyhow::anyhow!("Failed to create MCP server: {}", e))?;

    info!("MCP server ready, waiting for client on stdio...");

    // Start the MCP server on stdio
    let transport = rmcp::transport::io::stdio();

    let service = rmcp::ServiceExt::serve(mcp_server, transport)
        .await
        .map_err(|e| anyhow::anyhow!("MCP server initialization failed: {}", e))?;

    // Wait for the service to complete (client disconnects or error)
    service
        .waiting()
        .await
        .map_err(|e| anyhow::anyhow!("MCP server error: {}", e))?;

    info!("MCP server shutting down");
    Ok(())
}
