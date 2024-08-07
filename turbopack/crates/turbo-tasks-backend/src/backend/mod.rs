mod helpers;
mod operation;
mod storage;

use std::{
    borrow::Cow,
    collections::HashSet,
    future::Future,
    hash::BuildHasherDefault,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::Result;
use auto_hash_map::{AutoMap, AutoSet};
use dashmap::DashMap;
use parking_lot::{Condvar, Mutex};
use rustc_hash::FxHasher;
use smallvec::smallvec;
use turbo_tasks::{
    backend::{
        Backend, BackendJobId, CachedTaskType, CellContent, TaskExecutionSpec, TransientTaskType,
        TypedCellContent,
    },
    event::EventListener,
    registry,
    util::IdFactoryWithReuse,
    CellId, RawVc, TaskId, TraitTypeId, TurboTasksBackendApi, ValueTypeId, TRANSIENT_TASK_BIT,
};

use self::{
    operation::{AnyOperation, ExecuteContext},
    storage::Storage,
};
use crate::{
    data::{
        CachedDataItem, CachedDataItemKey, CachedDataItemValue, CachedDataUpdate, InProgressState,
        OutputValue,
    },
    get, remove,
    utils::{bi_map::BiMap, chunked_vec::ChunkedVec, ptr_eq_arc::PtrEqArc},
};

const SNAPSHOT_REQUESTED_BIT: usize = 1 << 63;

struct SnapshotRequest {
    snapshot_requested: bool,
    suspended_operations: HashSet<PtrEqArc<AnyOperation>>,
}

impl SnapshotRequest {
    fn new() -> Self {
        Self {
            snapshot_requested: false,
            suspended_operations: HashSet::new(),
        }
    }
}

pub struct TurboTasksBackend {
    persisted_task_id_factory: IdFactoryWithReuse<TaskId>,
    transient_task_id_factory: IdFactoryWithReuse<TaskId>,

    persisted_task_cache_log: Mutex<ChunkedVec<(Arc<CachedTaskType>, TaskId)>>,
    task_cache: BiMap<Arc<CachedTaskType>, TaskId>,
    transient_tasks: DashMap<TaskId, Arc<tokio::sync::Mutex<TransientTaskType>>>,

    persisted_storage_log: Mutex<ChunkedVec<CachedDataUpdate>>,
    storage: Storage<TaskId, CachedDataItem>,

    /// Number of executing operations + Highest bit is set when snapshot is
    /// requested. When that bit is set, operations should pause until the
    /// snapshot is completed. When the bit is set and in progress counter
    /// reaches zero, `operations_completed_when_snapshot_requested` is
    /// triggered.
    in_progress_operations: AtomicUsize,

    snapshot_request: Mutex<SnapshotRequest>,
    /// Condition Variable that is triggered when `in_progress_operations`
    /// reaches zero while snapshot is requested. All operations are either
    /// completed or suspended.
    operations_suspended: Condvar,
    /// Condition Variable that is triggered when a snapshot is completed and
    /// operations can continue.
    snapshot_completed: Condvar,
}

impl TurboTasksBackend {
    pub fn new() -> Self {
        Self {
            persisted_task_id_factory: IdFactoryWithReuse::new_with_range(
                1,
                (TRANSIENT_TASK_BIT - 1) as u64,
            ),
            transient_task_id_factory: IdFactoryWithReuse::new_with_range(
                TRANSIENT_TASK_BIT as u64,
                u32::MAX as u64,
            ),
            persisted_task_cache_log: Mutex::new(ChunkedVec::new()),
            task_cache: BiMap::new(),
            transient_tasks: DashMap::new(),
            persisted_storage_log: Mutex::new(ChunkedVec::new()),
            storage: Storage::new(),
            in_progress_operations: AtomicUsize::new(0),
            snapshot_request: Mutex::new(SnapshotRequest::new()),
            operations_suspended: Condvar::new(),
            snapshot_completed: Condvar::new(),
        }
    }

    fn execute_context<'a>(
        &'a self,
        turbo_tasks: &'a dyn TurboTasksBackendApi<Self>,
    ) -> ExecuteContext<'a> {
        ExecuteContext::new(self, turbo_tasks)
    }

    fn suspending_requested(&self) -> bool {
        (self.in_progress_operations.load(Ordering::Relaxed) & SNAPSHOT_REQUESTED_BIT) != 0
    }

    fn operation_suspend_point(&self, suspend: impl FnOnce() -> AnyOperation) {
        if (self.in_progress_operations.load(Ordering::Relaxed) & SNAPSHOT_REQUESTED_BIT) != 0 {
            let operation = Arc::new(suspend());
            let mut snapshot_request = self.snapshot_request.lock();
            if snapshot_request.snapshot_requested {
                snapshot_request
                    .suspended_operations
                    .insert(operation.clone().into());
                let value = self.in_progress_operations.fetch_sub(1, Ordering::AcqRel);
                assert!((value & SNAPSHOT_REQUESTED_BIT) != 0);
                if value == SNAPSHOT_REQUESTED_BIT {
                    self.operations_suspended.notify_all();
                }
                self.snapshot_completed
                    .wait_while(&mut snapshot_request, |snapshot_request| {
                        snapshot_request.snapshot_requested
                    });
                self.in_progress_operations.fetch_add(1, Ordering::AcqRel);
                snapshot_request
                    .suspended_operations
                    .remove(&operation.into());
            }
        }
    }
}

// Operations
impl TurboTasksBackend {
    pub fn connect_child(
        &self,
        parent_task: TaskId,
        child_task: TaskId,
        turbo_tasks: &dyn TurboTasksBackendApi<Self>,
    ) {
        operation::ConnectChildOperation::run(
            parent_task,
            child_task,
            self.execute_context(turbo_tasks),
        );
    }

    pub fn update_cell(
        &self,
        task_id: TaskId,
        cell: CellId,
        content: CellContent,
        turbo_tasks: &dyn TurboTasksBackendApi<Self>,
    ) {
        operation::UpdateCellOperation::run(
            task_id,
            cell,
            content,
            self.execute_context(turbo_tasks),
        );
    }

    pub fn invalidate(&self, task_id: TaskId, turbo_tasks: &dyn TurboTasksBackendApi<Self>) {
        operation::InvalidateOperation::run(smallvec![task_id], self.execute_context(turbo_tasks));
    }
}

impl Backend for TurboTasksBackend {
    fn get_or_create_persistent_task(
        &self,
        task_type: CachedTaskType,
        parent_task: TaskId,
        turbo_tasks: &dyn TurboTasksBackendApi<Self>,
    ) -> TaskId {
        if let Some(task_id) = self.task_cache.lookup_forward(&task_type) {
            self.connect_child(parent_task, task_id, turbo_tasks);
            return task_id;
        }

        let task_type = Arc::new(task_type);
        let task_id = self.persisted_task_id_factory.get();
        if let Err(existing_task_id) = self.task_cache.try_insert(task_type.clone(), task_id) {
            // Safety: We just created the id and failed to insert it.
            unsafe {
                self.persisted_task_id_factory.reuse(task_id);
            }
            self.connect_child(parent_task, existing_task_id, turbo_tasks);
            return existing_task_id;
        }
        self.persisted_task_cache_log
            .lock()
            .push((task_type, task_id));

        self.connect_child(parent_task, task_id, turbo_tasks);

        task_id
    }

    fn invalidate_task(&self, task_id: TaskId, turbo_tasks: &dyn TurboTasksBackendApi<Self>) {
        self.invalidate(task_id, turbo_tasks);
    }

    fn invalidate_tasks(&self, tasks: &[TaskId], turbo_tasks: &dyn TurboTasksBackendApi<Self>) {
        operation::InvalidateOperation::run(
            tasks.iter().copied().collect(),
            self.execute_context(turbo_tasks),
        );
    }

    fn invalidate_tasks_set(
        &self,
        tasks: &AutoSet<TaskId, BuildHasherDefault<FxHasher>, 2>,
        turbo_tasks: &dyn TurboTasksBackendApi<Self>,
    ) {
        operation::InvalidateOperation::run(
            tasks.iter().copied().collect(),
            self.execute_context(turbo_tasks),
        );
    }

    fn get_task_description(&self, task: TaskId) -> std::string::String {
        let task_type = self
            .task_cache
            .lookup_reverse(&task)
            .expect("Task not found");
        task_type.to_string()
    }

    type ExecutionScopeFuture<T: Future<Output = Result<()>>> = T where T: Send + 'static;
    fn execution_scope<T: Future<Output = Result<()>>>(
        &self,
        _: TaskId,
        future: T,
    ) -> Self::ExecutionScopeFuture<T>
    where
        T: Send + 'static,
    {
        future
    }

    fn try_start_task_execution(
        &self,
        task_id: TaskId,
        turbo_tasks: &dyn TurboTasksBackendApi<Self>,
    ) -> Option<TaskExecutionSpec<'_>> {
        {
            let ctx = self.execute_context(turbo_tasks);
            let mut task = ctx.task(task_id);
            let Some(InProgressState::Scheduled {
                clean,
                done_event,
                start_event,
            }) = remove!(task, InProgress)
            else {
                return None;
            };
            task.add(CachedDataItem::InProgress {
                value: InProgressState::InProgress {
                    clean,
                    stale: false,
                    done_event,
                },
            });
            start_event.notify(usize::MAX);
        }
        let (span, future) = if let Some(task_type) = self.task_cache.lookup_reverse(&task_id) {
            match &*task_type {
                CachedTaskType::Native { fn_type, this, arg } => (
                    registry::get_function(*fn_type).span(),
                    registry::get_function(*fn_type).execute(*this, &**arg),
                ),
                CachedTaskType::ResolveNative { fn_type, .. } => {
                    let span = registry::get_function(*fn_type).resolve_span();
                    let turbo_tasks = turbo_tasks.pin();
                    (
                        span,
                        Box::pin(async move {
                            let CachedTaskType::ResolveNative { fn_type, this, arg } = &*task_type
                            else {
                                unreachable!()
                            };
                            CachedTaskType::run_resolve_native(*fn_type, *this, &**arg, turbo_tasks)
                                .await
                        }) as Pin<Box<dyn Future<Output = _> + Send + '_>>,
                    )
                }
                CachedTaskType::ResolveTrait {
                    trait_type,
                    method_name,
                    ..
                } => {
                    let span = registry::get_trait(*trait_type).resolve_span(&**method_name);
                    let turbo_tasks = turbo_tasks.pin();
                    (
                        span,
                        Box::pin(async move {
                            let CachedTaskType::ResolveTrait {
                                trait_type,
                                method_name,
                                this,
                                arg,
                            } = &*task_type
                            else {
                                unreachable!()
                            };
                            CachedTaskType::run_resolve_trait(
                                *trait_type,
                                method_name.clone(),
                                *this,
                                &**arg,
                                turbo_tasks,
                            )
                            .await
                        }) as Pin<Box<dyn Future<Output = _> + Send + '_>>,
                    )
                }
            }
        } else if let Some(task_type) = self.transient_tasks.get(&task_id) {
            let task_type = task_type.clone();
            let span = tracing::trace_span!("turbo_tasks::root_task");
            let future = Box::pin(async move {
                let mut task_type = task_type.lock().await;
                match &mut *task_type {
                    TransientTaskType::Root(f) => {
                        let future = f();
                        drop(task_type);
                        future.await
                    }
                    TransientTaskType::Once(future) => future.await,
                }
            }) as Pin<Box<dyn Future<Output = _> + Send + '_>>;
            (span, future)
        } else {
            return None;
        };
        Some(TaskExecutionSpec { future, span })
    }

    fn task_execution_result(
        &self,
        task_id: TaskId,
        result: Result<Result<RawVc>, Option<Cow<'static, str>>>,
        turbo_tasks: &dyn TurboTasksBackendApi<Self>,
    ) {
        operation::UpdateOutputOperation::run(task_id, result, self.execute_context(turbo_tasks));
    }

    fn task_execution_completed(
        &self,
        task_id: TaskId,
        duration: Duration,
        memory_usage: usize,
        cell_counters: AutoMap<ValueTypeId, u32, BuildHasherDefault<FxHasher>, 8>,
        stateful: bool,
        turbo_tasks: &dyn TurboTasksBackendApi<Self>,
    ) -> bool {
        let ctx = self.execute_context(turbo_tasks);
        let mut task = ctx.task(task_id);
        let Some(CachedDataItemValue::InProgress { value: in_progress }) =
            task.remove(&CachedDataItemKey::InProgress {})
        else {
            panic!("Task execution completed, but task is not in progress");
        };
        let InProgressState::InProgress {
            done_event,
            clean,
            stale,
        } = in_progress
        else {
            panic!("Task execution completed, but task is not in progress");
        };

        // TODO handle cell counters

        if stale {
            task.add(CachedDataItem::InProgress {
                value: InProgressState::InProgress {
                    clean: false,
                    stale: false,
                    done_event,
                },
            });
        } else {
            done_event.notify(usize::MAX);
        }

        stale
    }

    fn run_backend_job(
        &self,
        _: BackendJobId,
        _: &dyn TurboTasksBackendApi<Self>,
    ) -> Pin<Box<(dyn Future<Output = ()> + Send + 'static)>> {
        todo!()
    }
    fn try_read_task_output(
        &self,
        _: TaskId,
        _: TaskId,
        _: bool,
        _: &dyn TurboTasksBackendApi<Self>,
    ) -> Result<Result<RawVc, EventListener>> {
        todo!()
    }
    fn try_read_task_output_untracked(
        &self,
        task_id: TaskId,
        strongy_consistent: bool,
        turbo_tasks: &dyn TurboTasksBackendApi<Self>,
    ) -> Result<Result<RawVc, EventListener>> {
        let ctx = self.execute_context(turbo_tasks);
        let task = ctx.task(task_id);

        if let Some(in_progress) = get!(task, InProgress) {
            match in_progress {
                InProgressState::Scheduled { done_event, .. }
                | InProgressState::InProgress { done_event, .. } => {
                    let listener = done_event.listen();
                    return Ok(Err(listener));
                }
            }
        }

        if strongy_consistent {
            todo!("Handle strongly consistent read");
        }

        if let Some(output) = get!(task, Output) {
            match output {
                OutputValue::Cell(cell) => return Ok(Ok(RawVc::TaskCell(cell.task, cell.cell))),
                OutputValue::Output(task) => return Ok(Ok(RawVc::TaskOutput(*task))),
                OutputValue::Error | OutputValue::Panic => {
                    if let Some(error) = get!(task, Error) {
                        return Err(error.clone().into());
                    }
                }
            }
        }

        todo!("Output is not available, recompute task");
    }
    fn try_read_task_cell(
        &self,
        _: TaskId,
        _: CellId,
        _: TaskId,
        _: &dyn TurboTasksBackendApi<Self>,
    ) -> Result<Result<TypedCellContent, EventListener>> {
        todo!()
    }

    fn try_read_task_cell_untracked(
        &self,
        task_id: TaskId,
        cell: CellId,
        turbo_tasks: &dyn TurboTasksBackendApi<Self>,
    ) -> Result<Result<TypedCellContent, EventListener>> {
        let ctx = self.execute_context(turbo_tasks);
        let task = ctx.task(task_id);
        if let Some(content) = get!(task, CellData { cell }) {
            return Ok(Ok(
                CellContent(Some(content.clone())).into_typed(cell.type_id)
            ));
        }
        todo!("Cell is not available, recompute task or error");
    }

    fn read_task_collectibles(
        &self,
        _: TaskId,
        _: TraitTypeId,
        _: TaskId,
        _: &dyn TurboTasksBackendApi<Self>,
    ) -> AutoMap<RawVc, i32, BuildHasherDefault<FxHasher>, 1> {
        todo!()
    }

    fn emit_collectible(
        &self,
        _: TraitTypeId,
        _: RawVc,
        _: TaskId,
        _: &dyn TurboTasksBackendApi<Self>,
    ) {
        todo!()
    }

    fn unemit_collectible(
        &self,
        _: TraitTypeId,
        _: RawVc,
        _: u32,
        _: TaskId,
        _: &dyn TurboTasksBackendApi<Self>,
    ) {
        todo!()
    }

    fn update_task_cell(
        &self,
        task_id: TaskId,
        cell: CellId,
        content: CellContent,
        turbo_tasks: &dyn TurboTasksBackendApi<Self>,
    ) {
        self.update_cell(task_id, cell, content, turbo_tasks);
    }

    fn get_or_create_transient_task(
        &self,
        _: CachedTaskType,
        _: TaskId,
        _: &dyn TurboTasksBackendApi<Self>,
    ) -> TaskId {
        todo!()
    }
    fn connect_task(&self, _: TaskId, _: TaskId, _: &dyn TurboTasksBackendApi<Self>) {
        todo!()
    }
    fn create_transient_task(
        &self,
        task_type: TransientTaskType,
        turbo_tasks: &dyn TurboTasksBackendApi<Self>,
    ) -> TaskId {
        let task_id = self.transient_task_id_factory.get();
        self.transient_tasks
            .insert(task_id, Arc::new(tokio::sync::Mutex::new(task_type)));
        {
            let mut task = self.storage.access_mut(task_id);
            task.add(CachedDataItem::new_scheduled(task_id));
        }
        turbo_tasks.schedule(task_id);
        task_id
    }
    fn dispose_root_task(&self, _: TaskId, _: &dyn TurboTasksBackendApi<Self>) {
        todo!()
    }
}
