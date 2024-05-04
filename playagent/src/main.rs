use std::{
    borrow::Borrow,
    collections::BTreeMap,
    net::SocketAddr,
    sync::Arc,
    thread::{self, JoinHandle},
};

use axum::{
    extract::State, http::StatusCode, response::IntoResponse, routing::{get, post}, Json
};
use clap::{error::ErrorKind, CommandFactory, Parser};
use crossbeam::{channel::Receiver, select};
use parking_lot::Mutex;
use playground::{core, supervisor};
use tracing::level_filters::LevelFilter;

#[derive(Debug, Parser)]
#[command(version, about, long_about = None)]
#[command(propagate_version = true)]
struct Cli {
    #[clap(
        long = "listen",
        short = 'l',
        help = "listen address for the agent.",
        default_value = "0.0.0.0:7777"
    )]
    listen: SocketAddr,

    #[clap(
        long = "vxlan-device",
        short = 'd',
        help = "device to use for vxlan tunnelling
the device needs to support multicast 
and be in the same network as the rest of the devices that are discovered by other agents.
"
    )]
    vxlan_device: String,
}

#[tokio::main]
async fn main() {
    if let Err(e) = tracing::subscriber::set_global_default(
        tracing_subscriber::FmtSubscriber::builder()
            .with_env_filter(
                tracing_subscriber::EnvFilter::builder()
                    .with_default_directive(LevelFilter::INFO.into())
                    .from_env_lossy(),
            )
            .finish(),
    ) {
        Cli::command()
            .error(
                ErrorKind::Io,
                format!("failed to set global default subscriber: {:?}", e),
            )
            .exit();
    }

    let cli: Cli = Cli::parse();
    let app = match app(&cli) {
        Ok(app) => app,
        Err(e) => {
            Cli::command()
                .error(ErrorKind::Io, format!("failed to create app: {:?}", e))
                .exit();
        }
    };
    match run(cli.listen, app).await {
        Ok(_) => {
            tracing::info!("server stopped");
        }
        Err(e) => {
            Cli::command()
                .error(ErrorKind::Io, format!("server exited with error: {:?}", e))
                .exit();
        }
    }
}

fn app(opts: &Cli) -> anyhow::Result<axum::Router> {
    let name = hostname::get()?
        .into_string()
        .map_err(|err| anyhow::anyhow!("{:?}", err))?;

    let host = Arc::new(HostInfo {
        hostname: name,
        vxlan_device: opts.vxlan_device.clone(),
    });
    let data = Mutex::new(Arc::new(Data::new()));

    let state = AppState {
        host,
        data,
        worker: Mutex::new(Worker {
            handle: None,
            interrupt: None,
            failure: None,
        }),
    };

    let router = axum::Router::new()
        .route("/host", get(get_host_info))
        .route("/network", get(get_network_state))
        .route("/network", post(set_network_state))
        .route("/worker/stop", post(worker_stop))
        .route("/worker/run", post(worker_run))
        .route("/worker/status", get(worker_status))
        .with_state(Arc::new(state));
    Ok(router)
}

#[derive(Debug)]
struct AppState {
    host: Arc<HostInfo>,
    data: Mutex<Arc<Data>>,
    worker: Mutex<Worker>,
}

type Data = playagent::Data;
type HostInfo = playagent::HostInfo;
type WorkerStatus = playagent::WorkerStatus;

#[derive(Debug)]
struct Worker {
    handle: Option<JoinHandle<anyhow::Result<()>>>,
    interrupt: Option<crossbeam::channel::Sender<()>>,
    failure: Option<anyhow::Result<()>>,
}

impl Worker {
    fn status(&self) -> WorkerStatus {
        match (&self.handle, &self.interrupt, &self.failure) {
            (None, _, None) => WorkerStatus::Pending,
            (Some(_), Some(_), None) => WorkerStatus::Running,
            (Some(handle), None, _) if handle.is_finished() => WorkerStatus::Stopped,
            (Some(_), None, _) => WorkerStatus::Stopping,
            (_, _, Some(_)) => WorkerStatus::Failed,
        }
    }
}


async fn get_host_info(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Arc<HostInfo>>, StatusCode> {
    Ok(Json(state.host.clone()))
}

async fn get_network_state(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Arc<Data>>, StatusCode> {
    Ok(Json(state.data.lock().clone()))
}


async fn set_network_state(
    State(state): State<Arc<AppState>>,
    Json(data): Json<Data>,
) -> impl IntoResponse {
    let worker = state.worker.lock();
    match worker.status() {
        WorkerStatus::Pending => {
            *state.data.lock() = Arc::new(data);
            (StatusCode::OK, Json(worker.status()))
        }
        _ => {
            tracing::debug!("worker is not in pending state, actual state is {:?}", worker.status());
            (StatusCode::CONFLICT, Json(worker.status()))
        }
    }
}

async fn worker_stop(State(state): State<Arc<AppState>>) -> Result<Json<WorkerStatus>, StatusCode> {
    let mut worker = state.worker.lock();
    let _ = worker.interrupt.take().map(|sender| sender.try_send(()));
    Ok(Json(worker.status()))
}

async fn worker_run(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut worker = state.worker.lock();
    match worker.status() {
        WorkerStatus::Pending => {
            let data = state.data.lock().clone();
            let (sender_interrupt, receiver_interrupt) = crossbeam::channel::bounded(1);
            worker.handle = Some(thread::spawn(move || {
                spawn_worker(&data, receiver_interrupt)
            }));
            worker.interrupt = Some(sender_interrupt);
            (StatusCode::OK, Json(worker.status()))
        }
        _ => {
            tracing::debug!("worker is not in pending state, actual state is {:?}", worker.status());
            (StatusCode::CONFLICT, Json(worker.status()))
        }
    }
}

fn spawn_worker(data: &Data, interrupt: Receiver<()>) -> anyhow::Result<()> {
    let (sender_errors, receiver_errors) = crossbeam::channel::unbounded();
    let mut running = BTreeMap::new();
    let mut results: Vec<anyhow::Error> = vec![];
    match core::deploy(data.network.borrow()) {
        Ok(()) => {
            match supervisor::launch(data.commands.borrow(), &mut running, &sender_errors) {
                Ok(()) => {
                    select! {
                        recv(interrupt) -> _ => {
                        }
                        recv(receiver_errors) -> err => {
                            if let Ok(err) = err {
                                results.push(anyhow::anyhow!("error in worker: {:?}", err));
                            }
                        }
                    }
                }
                Err(err) => {
                    results.push(err);
                }
            };
            if let Err(err) = supervisor::stop(&mut running) {
                results.push(err);
            }
        }
        Err(err) => results.push(err),
    }
    if let Err(err) = core::cleanup(data.network.borrow()) {
        results.push(err);
    }
    match results.len() {
        0 => Ok(()),
        _ => Err(anyhow::anyhow!("{:?}", results)),
    }
}

async fn worker_status(
    State(state): State<Arc<AppState>>,
) -> Result<Json<WorkerStatus>, StatusCode> {
    let mut worker = state.worker.lock();
    match worker.status() {
        WorkerStatus::Stopped => {
            let rst = worker.handle.take().map(|handle| handle.join());
            match rst {
                Some(Ok(Ok(_))) => {}
                Some(Ok(Err(e))) => worker.failure = Some(Err(e)),
                Some(Err(e)) => worker.failure = Some(Err(anyhow::anyhow!("{:?}", e))),
                None => {},
            }
        }
        _ => {}
    }
    Ok(Json(worker.status()))
}

async fn run(socket: SocketAddr, app: axum::Router) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(socket).await?;
    tracing::info!("listening on: {}", listener.local_addr()?);
    axum::serve(listener, app).await?;
    Ok(())
}
