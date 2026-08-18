//! `bottles.bottle.v1.Bottle` — a thin gRPC facade over `next-core`'s
//! `bottle` module (`BottleManager`, `Bottle`, `BottleEdit`). Every method
//! here unwraps its proto request, calls straight into `next-core`, and
//! maps the result back to proto types and `tonic::Status`; the actual
//! bottle/prefix/component/process logic all lives there, not here.

use std::pin::Pin;

use bottles_core::{
    self as core, Bottle, BottleManager,
    error::{BottleError, Error as CoreError},
};
use futures_core::Stream;
use next_proto::{
    bottles::bottle::v1::{
        self as proto, BottleRequest, BottleState, Component, CreateBottleEvent,
        CreateBottleRequest, CreateSnapshotEvent, CreateSnapshotRequest, DeleteBottleEvent,
        DeleteBottleRequest, Dependency, EditBottleRequest, EnvVar, GamescopeConfig,
        InstallDependencyEvent, InstallDependencyRequest, KillProcessRequest, ListBottlesResponse,
        ListProcessesResponse, ListSnapshotsResponse, MangoHudConfig, OperationProgress, Program,
        RemoveComponentEvent, RemoveComponentRequest, Requirement, RollbackEvent, RollbackRequest,
        RunProgramRequest, RunProgramResponse, SetComponentEvent, SetComponentRequest,
        SetDllOverrideRequest, Snapshot, SnapshotSummary, Transfer, UnsetDllOverrideRequest,
        bottle_server, create_bottle_event, create_snapshot_event, delete_bottle_event,
        edit_operation, install_dependency_event, remove_component_event, requirement,
        rollback_event, set_component_event,
    },
    winebridge::ListDllOverridesResponse,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, async_trait};
use uuid::Uuid;

pub struct BottleService {
    manager: BottleManager,
}

impl BottleService {
    pub fn new(manager: BottleManager) -> Self {
        Self { manager }
    }

    async fn open(&self, id: &str) -> Result<Bottle, Status> {
        let id = Uuid::parse_str(id).map_err(|_| Status::invalid_argument("invalid bottle_id"))?;
        self.manager.open(id).await.map_err(to_status)
    }
}

/// Maps a `next-core` error to the closest `tonic::Status`. `Error::Status`
/// (surfaced from WineBridge calls) is passed through as-is.
fn to_status(err: CoreError) -> Status {
    let message = err.to_string();
    match &err {
        CoreError::Status(status) => status.clone(),
        CoreError::Bottle(
            BottleError::NotFound(_) | BottleError::Deleted(_) | BottleError::ProgramNotFound(_),
        ) => Status::not_found(message),
        CoreError::Bottle(
            BottleError::ComponentNotInstalled(_) | BottleError::RequiresAddon { .. },
        ) => Status::failed_precondition(message),
        CoreError::Bottle(
            BottleError::InvalidProgram
            | BottleError::InvalidEnvironmentName(_)
            | BottleError::InvalidEnvironmentValue(_)
            | BottleError::InvalidDllName(_)
            | BottleError::DllOverrideModeRequired
            | BottleError::InvalidComponentSlot { .. }
            | BottleError::IdMismatch { .. },
        ) => Status::invalid_argument(message),
        CoreError::Addon(core::AddonError::NotFound(_)) => Status::not_found(message),
        CoreError::Cancelled => Status::cancelled(message),
        _ => Status::internal(message),
    }
}

fn slot_to_proto(slot: core::Slot) -> proto::Slot {
    match slot {
        core::Slot::WineBridge => proto::Slot::Winebridge,
        core::Slot::Runner => proto::Slot::Runner,
        core::Slot::Umu => proto::Slot::Umu,
        core::Slot::Dxvk => proto::Slot::Dxvk,
        core::Slot::Vkd3d => proto::Slot::Vkd3d,
        core::Slot::Nvapi => proto::Slot::Nvapi,
        core::Slot::LatencyFlex => proto::Slot::LatencyFlex,
    }
}

fn slot_from_proto(slot: proto::Slot) -> Result<core::Slot, Status> {
    match slot {
        proto::Slot::Winebridge => Ok(core::Slot::WineBridge),
        proto::Slot::Runner => Ok(core::Slot::Runner),
        proto::Slot::Umu => Ok(core::Slot::Umu),
        proto::Slot::Dxvk => Ok(core::Slot::Dxvk),
        proto::Slot::Vkd3d => Ok(core::Slot::Vkd3d),
        proto::Slot::Nvapi => Ok(core::Slot::Nvapi),
        proto::Slot::LatencyFlex => Ok(core::Slot::LatencyFlex),
        proto::Slot::Unspecified => Err(Status::invalid_argument("slot is required")),
    }
}

fn storage_to_proto(storage: core::Storage) -> proto::Storage {
    match storage {
        core::Storage::Standard => proto::Storage::Standard,
        core::Storage::Virgo => proto::Storage::Virgo,
    }
}

fn storage_from_proto(storage: proto::Storage) -> Result<core::Storage, Status> {
    match storage {
        proto::Storage::Standard => Ok(core::Storage::Standard),
        proto::Storage::Virgo => Ok(core::Storage::Virgo),
        proto::Storage::Unspecified => Err(Status::invalid_argument("storage is required")),
    }
}

fn requirement_to_proto(requirement: &core::Requirement) -> Requirement {
    let kind = match requirement {
        core::Requirement::Name(name) => requirement::Kind::Name(name.clone()),
        core::Requirement::Slot(slot) => requirement::Kind::Slot(slot_to_proto(*slot) as i32),
        core::Requirement::Id(id) => requirement::Kind::Id(id.to_string()),
    };
    Requirement { kind: Some(kind) }
}

fn component_to_proto(component: &core::Addon<core::Component>) -> Component {
    Component {
        id: component.id().to_string(),
        name: component.name().to_string(),
        version: component.version().to_string(),
        requirements: component
            .requirements()
            .iter()
            .map(requirement_to_proto)
            .collect(),
        slot: slot_to_proto(component.slot()) as i32,
    }
}

fn dependency_to_proto(dependency: &core::Addon<core::Dependency>) -> Dependency {
    Dependency {
        id: dependency.id().to_string(),
        name: dependency.name().to_string(),
        version: dependency.version().to_string(),
        requirements: dependency
            .requirements()
            .iter()
            .map(requirement_to_proto)
            .collect(),
    }
}

fn program_to_proto(program: &core::Program) -> Program {
    Program {
        id: program.id.to_string(),
        name: program.name.clone(),
        executable: program.executable.clone(),
        args: program.args.clone(),
        working_directory: program.working_directory.clone(),
        new_console: program.new_console,
    }
}

fn program_from_proto(program: Program) -> core::Program {
    core::Program {
        id: Uuid::parse_str(&program.id).unwrap_or_else(|_| Uuid::new_v4()),
        name: program.name,
        executable: program.executable,
        args: program.args,
        working_directory: program.working_directory,
        new_console: program.new_console,
    }
}

fn scaler_to_proto(scaler: core::GamescopeScaler) -> proto::Scaler {
    match scaler {
        core::GamescopeScaler::Auto => proto::Scaler::Auto,
        core::GamescopeScaler::Integer => proto::Scaler::Integer,
        core::GamescopeScaler::Fit => proto::Scaler::Fit,
        core::GamescopeScaler::Fill => proto::Scaler::Fill,
        core::GamescopeScaler::Stretch => proto::Scaler::Stretch,
    }
}

fn scaler_from_proto(scaler: proto::Scaler) -> Option<core::GamescopeScaler> {
    match scaler {
        proto::Scaler::Unspecified => None,
        proto::Scaler::Auto => Some(core::GamescopeScaler::Auto),
        proto::Scaler::Integer => Some(core::GamescopeScaler::Integer),
        proto::Scaler::Fit => Some(core::GamescopeScaler::Fit),
        proto::Scaler::Fill => Some(core::GamescopeScaler::Fill),
        proto::Scaler::Stretch => Some(core::GamescopeScaler::Stretch),
    }
}

fn filter_to_proto(filter: core::GamescopeFilter) -> proto::Filter {
    match filter {
        core::GamescopeFilter::Linear => proto::Filter::Linear,
        core::GamescopeFilter::Nearest => proto::Filter::Nearest,
        core::GamescopeFilter::Fsr => proto::Filter::Fsr,
        core::GamescopeFilter::Nis => proto::Filter::Nis,
        core::GamescopeFilter::Pixel => proto::Filter::Pixel,
    }
}

fn filter_from_proto(filter: proto::Filter) -> Option<core::GamescopeFilter> {
    match filter {
        proto::Filter::Unspecified => None,
        proto::Filter::Linear => Some(core::GamescopeFilter::Linear),
        proto::Filter::Nearest => Some(core::GamescopeFilter::Nearest),
        proto::Filter::Fsr => Some(core::GamescopeFilter::Fsr),
        proto::Filter::Nis => Some(core::GamescopeFilter::Nis),
        proto::Filter::Pixel => Some(core::GamescopeFilter::Pixel),
    }
}

fn gamescope_to_proto(config: &core::GamescopeConfig) -> GamescopeConfig {
    GamescopeConfig {
        enabled: config.enabled,
        game_width: config.game_width,
        game_height: config.game_height,
        output_width: config.output_width,
        output_height: config.output_height,
        frame_rate: config.frame_rate,
        unfocused_frame_rate: config.unfocused_frame_rate,
        scaler: config.scaler.map(|scaler| scaler_to_proto(scaler) as i32),
        filter: config.filter.map(|filter| filter_to_proto(filter) as i32),
        sharpness: config.sharpness.map(u32::from),
        borderless: config.borderless,
        fullscreen: config.fullscreen,
    }
}

fn gamescope_from_proto(config: GamescopeConfig) -> core::GamescopeConfig {
    core::GamescopeConfig {
        enabled: config.enabled,
        game_width: config.game_width,
        game_height: config.game_height,
        output_width: config.output_width,
        output_height: config.output_height,
        frame_rate: config.frame_rate,
        unfocused_frame_rate: config.unfocused_frame_rate,
        scaler: config
            .scaler
            .and_then(|s| proto::Scaler::try_from(s).ok())
            .and_then(scaler_from_proto),
        filter: config
            .filter
            .and_then(|f| proto::Filter::try_from(f).ok())
            .and_then(filter_from_proto),
        sharpness: config.sharpness.and_then(|s| u8::try_from(s).ok()),
        borderless: config.borderless,
        fullscreen: config.fullscreen,
    }
}

fn mangohud_to_proto(config: &core::MangoHudConfig) -> MangoHudConfig {
    MangoHudConfig {
        enabled: config.enabled,
    }
}

fn mangohud_from_proto(config: MangoHudConfig) -> core::MangoHudConfig {
    core::MangoHudConfig {
        enabled: config.enabled,
    }
}

fn bottle_state_to_proto(state: &core::BottleState) -> BottleState {
    BottleState {
        id: state.id().to_string(),
        name: state.name().to_string(),
        storage: storage_to_proto(state.storage()) as i32,
        programs: state.programs().iter().map(program_to_proto).collect(),
        components: state
            .components()
            .iter()
            .map(|(slot, component)| (slot.as_str().to_string(), component_to_proto(component)))
            .collect(),
        dependencies: state
            .dependencies()
            .iter()
            .map(dependency_to_proto)
            .collect(),
        environment: state
            .environment()
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect(),
        gamescope: Some(gamescope_to_proto(&state.wrappers().gamescope)),
        mangohud: Some(mangohud_to_proto(&state.wrappers().mangohud)),
    }
}

fn convert_progress(progress: core::Progress) -> OperationProgress {
    let (stage, file) = match progress.stage {
        core::Stage::Preparing => (proto::Stage::Preparing, None),
        core::Stage::Stopping => (proto::Stage::Stopping, None),
        core::Stage::Downloading { file } => (proto::Stage::Downloading, Some(file)),
        core::Stage::Verifying { file } => (proto::Stage::Verifying, Some(file)),
        core::Stage::Extracting => (proto::Stage::Extracting, None),
        core::Stage::CreatingPrefix => (proto::Stage::CreatingPrefix, None),
        core::Stage::Checkpointing => (proto::Stage::Checkpointing, None),
        core::Stage::Restoring => (proto::Stage::Restoring, None),
        core::Stage::Rebuilding => (proto::Stage::Rebuilding, None),
        core::Stage::Configuring => (proto::Stage::Configuring, None),
        core::Stage::Removing => (proto::Stage::Removing, None),
        core::Stage::Committing => (proto::Stage::Committing, None),
    };
    OperationProgress {
        stage: stage as i32,
        file,
        transfer: progress.transfer.map(|transfer| Transfer {
            current: transfer.current,
            total: transfer.total,
        }),
    }
}

/// Drives an `Operation<T>` to completion, forwarding its progress and
/// terminal result into `tx` via the caller-supplied event constructors.
/// The progress stream naturally ends once the operation's future
/// completes and drops its progress sender (see `next-core`'s
/// `Operation::progress` docs), so this doesn't need to coordinate the
/// two loops explicitly beyond spawning them.
fn drive_operation<T, E>(
    operation: core::Operation<T>,
    tx: mpsc::Sender<Result<E, Status>>,
    wrap_progress: impl Fn(OperationProgress) -> E + Send + 'static,
    wrap_done: impl FnOnce(T) -> E + Send + 'static,
) where
    T: Send + 'static,
    E: Send + 'static,
{
    use tokio_stream::StreamExt;

    let progress_stream = operation.progress();
    let progress_tx = tx.clone();
    tokio::spawn(async move {
        tokio::pin!(progress_stream);
        while let Some(progress) = progress_stream.next().await {
            let event = wrap_progress(convert_progress(progress));
            if progress_tx.send(Ok(event)).await.is_err() {
                return;
            }
        }
    });

    tokio::spawn(async move {
        match operation.await {
            Ok(value) => {
                let _ = tx.send(Ok(wrap_done(value))).await;
            }
            Err(err) => {
                let _ = tx.send(Err(to_status(err))).await;
            }
        }
    });
}

type EventStream<E> = Pin<Box<dyn Stream<Item = Result<E, Status>> + Send + 'static>>;

#[async_trait]
impl bottle_server::Bottle for BottleService {
    type CreateBottleStream = EventStream<CreateBottleEvent>;

    async fn create_bottle(
        &self,
        request: Request<CreateBottleRequest>,
    ) -> Result<Response<Self::CreateBottleStream>, Status> {
        let req = request.into_inner();
        let storage =
            storage_from_proto(proto::Storage::try_from(req.storage).unwrap_or_default())?;
        let runner = Uuid::parse_str(&req.runner_component_id)
            .map_err(|_| Status::invalid_argument("invalid runner_component_id"))?;

        let operation = self.manager.create(req.name, storage, runner);
        let (tx, rx) = mpsc::channel(32);
        drive_operation(
            operation,
            tx,
            |progress| CreateBottleEvent {
                event: Some(create_bottle_event::Event::Progress(progress)),
            },
            |bottle| CreateBottleEvent {
                event: bottle
                    .state()
                    .ok()
                    .map(|state| create_bottle_event::Event::Bottle(bottle_state_to_proto(&state))),
            },
        );
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    type DeleteBottleStream = EventStream<DeleteBottleEvent>;

    async fn delete_bottle(
        &self,
        request: Request<DeleteBottleRequest>,
    ) -> Result<Response<Self::DeleteBottleStream>, Status> {
        let req = request.into_inner();
        let id = Uuid::parse_str(&req.bottle_id)
            .map_err(|_| Status::invalid_argument("invalid bottle_id"))?;

        let operation = self.manager.delete(id);
        let (tx, rx) = mpsc::channel(32);
        drive_operation(
            operation,
            tx,
            |progress| DeleteBottleEvent {
                event: Some(delete_bottle_event::Event::Progress(progress)),
            },
            |()| DeleteBottleEvent {
                event: Some(delete_bottle_event::Event::Done(())),
            },
        );
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    async fn get_bottle(
        &self,
        request: Request<BottleRequest>,
    ) -> Result<Response<BottleState>, Status> {
        let bottle = self.open(&request.into_inner().bottle_id).await?;
        let state = bottle.state().map_err(to_status)?;
        Ok(Response::new(bottle_state_to_proto(&state)))
    }

    async fn list_bottles(
        &self,
        _request: Request<()>,
    ) -> Result<Response<ListBottlesResponse>, Status> {
        let mut bottles = Vec::new();
        for bottle in self.manager.list() {
            if let Ok(state) = bottle.state() {
                bottles.push(bottle_state_to_proto(&state));
            }
        }
        Ok(Response::new(ListBottlesResponse { bottles }))
    }

    type WatchBottlesStream = EventStream<ListBottlesResponse>;

    async fn watch_bottles(
        &self,
        _request: Request<()>,
    ) -> Result<Response<Self::WatchBottlesStream>, Status> {
        use tokio_stream::StreamExt;

        let stream = self.manager.watch().map(|bottles| {
            let bottles = bottles
                .iter()
                .filter_map(|bottle| bottle.state().ok())
                .map(|state| bottle_state_to_proto(&state))
                .collect();
            Ok(ListBottlesResponse { bottles })
        });
        Ok(Response::new(Box::pin(stream)))
    }

    type WatchBottleStream = EventStream<BottleState>;

    async fn watch_bottle(
        &self,
        request: Request<BottleRequest>,
    ) -> Result<Response<Self::WatchBottleStream>, Status> {
        use tokio_stream::StreamExt;

        let bottle = self.open(&request.into_inner().bottle_id).await?;
        let stream = bottle
            .watch()
            .map(|state| Ok(bottle_state_to_proto(&state)));
        Ok(Response::new(Box::pin(stream)))
    }

    async fn edit_bottle(
        &self,
        request: Request<EditBottleRequest>,
    ) -> Result<Response<BottleState>, Status> {
        let req = request.into_inner();
        let bottle = self.open(&req.bottle_id).await?;
        let mut edit = bottle.edit();

        for operation in req.operations {
            let Some(change) = operation.change else {
                continue;
            };
            match change {
                edit_operation::Change::Rename(name) => {
                    edit.rename(name);
                }
                edit_operation::Change::SetEnv(EnvVar { key, value }) => {
                    edit.set_env(&key, &value);
                }
                edit_operation::Change::UnsetEnv(key) => {
                    edit.unset_env(&key);
                }
                edit_operation::Change::AddProgram(program) => {
                    edit.add_program(program_from_proto(program));
                }
                edit_operation::Change::RemoveProgramId(id) => {
                    let id = Uuid::parse_str(&id)
                        .map_err(|_| Status::invalid_argument("invalid remove_program_id"))?;
                    edit.remove_program(id);
                }
                edit_operation::Change::SetGamescope(config) => {
                    edit.set_gamescope(gamescope_from_proto(config));
                }
                edit_operation::Change::SetMangohud(config) => {
                    edit.set_mangohud(mangohud_from_proto(config));
                }
            }
        }

        edit.commit().await.map_err(to_status)?;
        let state = bottle.state().map_err(to_status)?;
        Ok(Response::new(bottle_state_to_proto(&state)))
    }

    type SetComponentStream = EventStream<SetComponentEvent>;

    async fn set_component(
        &self,
        request: Request<SetComponentRequest>,
    ) -> Result<Response<Self::SetComponentStream>, Status> {
        let req = request.into_inner();
        let bottle = self.open(&req.bottle_id).await?;
        let component_id = Uuid::parse_str(&req.component_id)
            .map_err(|_| Status::invalid_argument("invalid component_id"))?;

        let operation = bottle.set_component(component_id);
        let (tx, rx) = mpsc::channel(32);
        drive_operation(
            operation,
            tx,
            |progress| SetComponentEvent {
                event: Some(set_component_event::Event::Progress(progress)),
            },
            |()| SetComponentEvent {
                event: Some(set_component_event::Event::Done(())),
            },
        );
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    type RemoveComponentStream = EventStream<RemoveComponentEvent>;

    async fn remove_component(
        &self,
        request: Request<RemoveComponentRequest>,
    ) -> Result<Response<Self::RemoveComponentStream>, Status> {
        let req = request.into_inner();
        let bottle = self.open(&req.bottle_id).await?;
        let slot = slot_from_proto(proto::Slot::try_from(req.slot).unwrap_or_default())?;

        let operation = bottle.remove_component(slot);
        let (tx, rx) = mpsc::channel(32);
        drive_operation(
            operation,
            tx,
            |progress| RemoveComponentEvent {
                event: Some(remove_component_event::Event::Progress(progress)),
            },
            |()| RemoveComponentEvent {
                event: Some(remove_component_event::Event::Done(())),
            },
        );
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    type InstallDependencyStream = EventStream<InstallDependencyEvent>;

    async fn install_dependency(
        &self,
        request: Request<InstallDependencyRequest>,
    ) -> Result<Response<Self::InstallDependencyStream>, Status> {
        let req = request.into_inner();
        let bottle = self.open(&req.bottle_id).await?;
        let dependency_id = Uuid::parse_str(&req.dependency_id)
            .map_err(|_| Status::invalid_argument("invalid dependency_id"))?;

        let operation = bottle.install(dependency_id);
        let (tx, rx) = mpsc::channel(32);
        drive_operation(
            operation,
            tx,
            |progress| InstallDependencyEvent {
                event: Some(install_dependency_event::Event::Progress(progress)),
            },
            |()| InstallDependencyEvent {
                event: Some(install_dependency_event::Event::Done(())),
            },
        );
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    async fn run_program(
        &self,
        request: Request<RunProgramRequest>,
    ) -> Result<Response<RunProgramResponse>, Status> {
        let req = request.into_inner();
        let bottle = self.open(&req.bottle_id).await?;
        let program_id = Uuid::parse_str(&req.program_id)
            .map_err(|_| Status::invalid_argument("invalid program_id"))?;
        let pid = bottle.run(program_id).await.map_err(to_status)?;
        Ok(Response::new(RunProgramResponse { pid }))
    }

    async fn list_processes(
        &self,
        request: Request<BottleRequest>,
    ) -> Result<Response<ListProcessesResponse>, Status> {
        let bottle = self.open(&request.into_inner().bottle_id).await?;
        let processes = bottle.processes().await.map_err(to_status)?;
        Ok(Response::new(ListProcessesResponse { processes }))
    }

    async fn kill_process(
        &self,
        request: Request<KillProcessRequest>,
    ) -> Result<Response<()>, Status> {
        let req = request.into_inner();
        let bottle = self.open(&req.bottle_id).await?;
        let program_id = Uuid::parse_str(&req.program_id)
            .map_err(|_| Status::invalid_argument("invalid program_id"))?;
        bottle.kill(program_id).await.map_err(to_status)?;
        Ok(Response::new(()))
    }

    async fn stop_bottle(&self, request: Request<BottleRequest>) -> Result<Response<()>, Status> {
        let bottle = self.open(&request.into_inner().bottle_id).await?;
        bottle.stop().await.map_err(to_status)?;
        Ok(Response::new(()))
    }

    async fn list_dll_overrides(
        &self,
        request: Request<BottleRequest>,
    ) -> Result<Response<ListDllOverridesResponse>, Status> {
        let bottle = self.open(&request.into_inner().bottle_id).await?;
        let overrides = bottle.dll_overrides().await.map_err(to_status)?;
        Ok(Response::new(ListDllOverridesResponse { overrides }))
    }

    async fn set_dll_override(
        &self,
        request: Request<SetDllOverrideRequest>,
    ) -> Result<Response<()>, Status> {
        let req = request.into_inner();
        let bottle = self.open(&req.bottle_id).await?;
        let mode = next_proto::winebridge::DllOverrideMode::try_from(req.mode)
            .map_err(|_| Status::invalid_argument("invalid dll override mode"))?;
        bottle
            .set_dll_override(req.dll, mode)
            .await
            .map_err(to_status)?;
        Ok(Response::new(()))
    }

    async fn unset_dll_override(
        &self,
        request: Request<UnsetDllOverrideRequest>,
    ) -> Result<Response<()>, Status> {
        let req = request.into_inner();
        let bottle = self.open(&req.bottle_id).await?;
        bottle
            .unset_dll_override(req.dll)
            .await
            .map_err(to_status)?;
        Ok(Response::new(()))
    }

    type CreateSnapshotStream = EventStream<CreateSnapshotEvent>;

    async fn create_snapshot(
        &self,
        request: Request<CreateSnapshotRequest>,
    ) -> Result<Response<Self::CreateSnapshotStream>, Status> {
        let req = request.into_inner();
        let bottle = self.open(&req.bottle_id).await?;

        let operation = bottle.create_snapshot(req.message);
        let (tx, rx) = mpsc::channel(32);
        drive_operation(
            operation,
            tx,
            |progress| CreateSnapshotEvent {
                event: Some(create_snapshot_event::Event::Progress(progress)),
            },
            |snapshot| CreateSnapshotEvent {
                event: Some(create_snapshot_event::Event::Snapshot(Snapshot {
                    repository_path: snapshot.repository_path,
                    state_id: snapshot.state_id,
                    created_at: snapshot.created_at.map(|ts| prost_wkt_types::Timestamp {
                        seconds: ts.seconds,
                        nanos: ts.nanos,
                    }),
                    file_count: snapshot.file_count,
                    message: snapshot.message,
                    created: snapshot.created,
                })),
            },
        );
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    async fn list_snapshots(
        &self,
        request: Request<BottleRequest>,
    ) -> Result<Response<ListSnapshotsResponse>, Status> {
        let bottle = self.open(&request.into_inner().bottle_id).await?;
        let snapshots = bottle
            .snapshots()
            .await
            .map_err(to_status)?
            .into_iter()
            .map(|summary| SnapshotSummary {
                repository_path: summary.repository_path,
                state_id: summary.state_id,
                created_at: summary.created_at.map(|ts| prost_wkt_types::Timestamp {
                    seconds: ts.seconds,
                    nanos: ts.nanos,
                }),
                message: summary.message,
                file_count: summary.file_count,
            })
            .collect();
        Ok(Response::new(ListSnapshotsResponse { snapshots }))
    }

    type RollbackStream = EventStream<RollbackEvent>;

    async fn rollback(
        &self,
        request: Request<RollbackRequest>,
    ) -> Result<Response<Self::RollbackStream>, Status> {
        let req = request.into_inner();
        let bottle = self.open(&req.bottle_id).await?;

        let operation = bottle.rollback(&req.state_id_or_prefix);
        let (tx, rx) = mpsc::channel(32);
        drive_operation(
            operation,
            tx,
            |progress| RollbackEvent {
                event: Some(rollback_event::Event::Progress(progress)),
            },
            |state_id| RollbackEvent {
                event: Some(rollback_event::Event::StateId(state_id)),
            },
        );
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }
}
