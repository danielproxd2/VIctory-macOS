//! XTaskQueue and XAsync: the runtime's callback queues and async-call
//! blocks, served as the task-queue provider's object from `QueryApiImpl`.
//!
//! As documented for the public API: a queue has a work port and a
//! completion port, each dispatched manually (the title calls dispatch),
//! on a thread pool, serialised on one thread, or immediately on the
//! submitting thread. A null queue handle means the process queue. An
//! async block's system area holds its status and result size, so both
//! stay readable after our state for the call is gone.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard};
use std::time::Duration;

use super::xgameruntime::{Guid, HRESULT, Interface, Methods, Object, S_OK, add_ref, missing};

/// The task-queue provider; it is also the interface id titles ask for.
pub const PROVIDER: Guid = Guid {
    data1: 0x073b_7dcb,
    data2: 0x1fcf,
    data3: 0x4030,
    data4: [0x94, 0xbe, 0xe3, 0xc9, 0xeb, 0x62, 0x34, 0x28],
};

pub const E_PENDING: HRESULT = 0x8000_000A_u32 as i32;
pub const E_ABORT: HRESULT = 0x8000_4004_u32 as i32;
pub const E_NOTIMPL: HRESULT = 0x8000_4001_u32 as i32;
pub const E_INVALIDARG: HRESULT = 0x8007_0057_u32 as i32;
pub const E_NOT_SUFFICIENT_BUFFER: HRESULT = 0x8007_007A_u32 as i32;
pub const E_ILLEGAL_METHOD_CALL: HRESULT = 0x8000_000E_u32 as i32;

const WORK: u32 = 0;
const COMPLETION: u32 = 1;
const INFINITE: u32 = u32::MAX;

type Callback = extern "system" fn(*mut c_void, bool);
type MonitorCallback = extern "system" fn(*mut c_void, *const Queue, u32);
type TerminatedCallback = extern "system" fn(*mut c_void);
type CompletionRoutine = extern "system" fn(*mut AsyncBlock);
type Provider = extern "system" fn(u32, *const ProviderData) -> HRESULT;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Manual,
    ThreadPool,
    Serialized,
    Immediate,
}

impl Mode {
    fn from_raw(mode: u32) -> Option<Mode> {
        [
            Mode::Manual,
            Mode::ThreadPool,
            Mode::Serialized,
            Mode::Immediate,
        ]
        .get(mode as usize)
        .copied()
    }
}

/// A title callback and its context (addresses, so tasks can cross threads).
#[derive(Clone, Copy)]
struct Task {
    context: usize,
    callback: usize,
}

impl Task {
    fn new(context: *mut c_void, callback: Callback) -> Task {
        Task {
            context: context as usize,
            callback: callback as usize,
        }
    }

    fn run(self, canceled: bool) {
        // SAFETY: built from a `Callback` in `new`.
        let callback: Callback = unsafe { std::mem::transmute(self.callback) };
        callback(self.context as *mut c_void, canceled);
    }
}

struct Port {
    mode: Mode,
    tasks: Mutex<VecDeque<Task>>,
    ready: Condvar,
}

impl Port {
    fn new(mode: Mode) -> Arc<Port> {
        let port = Arc::new(Port {
            mode,
            tasks: Mutex::default(),
            ready: Condvar::new(),
        });
        if mode == Mode::Serialized {
            // The worker ends once it holds the last reference (every queue
            // using the port is closed) and nothing is left to run.
            let worker = port.clone();
            std::thread::spawn(move || {
                loop {
                    match worker.take(1000, || false) {
                        Some(task) => task.run(false),
                        None if Arc::strong_count(&worker) == 1 => break,
                        None => {}
                    }
                }
            });
        }
        port
    }

    fn submit(&self, task: Task) {
        match self.mode {
            Mode::Immediate => task.run(false),
            Mode::ThreadPool => POOL.submit(task),
            Mode::Manual | Mode::Serialized => {
                lock(&self.tasks).push_back(task);
                self.ready.notify_one();
            }
        }
    }

    /// The next queued task, waiting up to `timeout_ms`, or until `ended`
    /// (checked whenever the port is woken) says no task will come.
    fn take(&self, timeout_ms: u32, ended: impl Fn() -> bool) -> Option<Task> {
        let waiting = |t: &mut VecDeque<Task>| t.is_empty() && !ended();
        let tasks = lock(&self.tasks);
        let mut tasks = if timeout_ms == INFINITE {
            self.ready
                .wait_while(tasks, waiting)
                .unwrap_or_else(|e| e.into_inner())
        } else {
            let timeout = Duration::from_millis(timeout_ms.into());
            self.ready
                .wait_timeout_while(tasks, timeout, waiting)
                .unwrap_or_else(|e| e.into_inner())
                .0
        };
        tasks.pop_front()
    }
}

/// The thread pool behind thread-pool ports: idle workers are reused, and a
/// new one starts whenever more tasks wait than workers idle, so a callback
/// that blocks on another never starves it. Workers stay for reuse.
struct Pool {
    /// Queued tasks and the number of idle workers.
    state: Mutex<(VecDeque<Task>, usize)>,
    ready: Condvar,
}

static POOL: Pool = Pool {
    state: Mutex::new((VecDeque::new(), 0)),
    ready: Condvar::new(),
};

impl Pool {
    fn submit(&'static self, task: Task) {
        let mut state = lock(&self.state);
        state.0.push_back(task);
        if state.0.len() > state.1 {
            std::thread::spawn(move || self.work());
        } else {
            self.ready.notify_one();
        }
    }

    fn work(&self) {
        let mut state = lock(&self.state);
        loop {
            while state.0.is_empty() {
                state.1 += 1;
                state = self.ready.wait(state).unwrap_or_else(|e| e.into_inner());
                state.1 -= 1;
            }
            let task = state.0.pop_front();
            drop(state);
            if let Some(task) = task {
                task.run(false);
            }
            state = lock(&self.state);
        }
    }
}

/// A queue handle is `Arc::into_raw` of this; duplicating and closing a
/// handle add and drop a reference.
pub struct Queue {
    ports: [Arc<Port>; 2],
    monitors: Mutex<Vec<(u64, usize, usize)>>,
    terminated: AtomicBool,
}

impl Queue {
    fn new(ports: [Arc<Port>; 2]) -> Arc<Queue> {
        Arc::new(Queue {
            ports,
            monitors: Mutex::default(),
            terminated: AtomicBool::new(false),
        })
    }

    fn submit(self: &Arc<Self>, port: u32, delay_ms: u32, task: Task) -> HRESULT {
        if port > COMPLETION {
            return E_INVALIDARG;
        }
        if self.terminated.load(Ordering::Acquire) {
            return E_ABORT;
        }
        if delay_ms == 0 {
            self.deliver(port, task);
        } else {
            let queue = self.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(delay_ms.into()));
                queue.deliver(port, task);
            });
        }
        S_OK
    }

    fn deliver(self: &Arc<Self>, port: u32, task: Task) {
        self.ports[port as usize].submit(task);
        // Copied out: a monitor may unregister itself.
        let monitors = lock(&self.monitors).clone();
        for (_, context, callback) in monitors {
            // SAFETY: stored from a `MonitorCallback` in `register_monitor`.
            let callback: MonitorCallback = unsafe { std::mem::transmute(callback) };
            callback(context as *mut c_void, Arc::as_ptr(self), port);
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The process queue: `None` until first used (then a thread-pool queue),
/// `Some(None)` once the title clears it.
static PROCESS_QUEUE: Mutex<Option<Option<Arc<Queue>>>> = Mutex::new(None);

fn process_queue() -> Option<Arc<Queue>> {
    lock(&PROCESS_QUEUE)
        .get_or_insert_with(|| {
            Some(Queue::new([
                Port::new(Mode::ThreadPool),
                Port::new(Mode::ThreadPool),
            ]))
        })
        .clone()
}

/// A new reference to the queue behind `handle` (null: the process queue).
fn queue(handle: *const Queue) -> Option<Arc<Queue>> {
    if handle.is_null() {
        return process_queue();
    }
    // SAFETY: a handle we returned, which the title has not closed.
    unsafe {
        Arc::increment_strong_count(handle);
        Some(Arc::from_raw(handle))
    }
}

pub fn XTaskQueueCreate(work: u32, completion: u32, out: *mut *const Queue) -> HRESULT {
    let (Some(work), Some(completion)) = (Mode::from_raw(work), Mode::from_raw(completion)) else {
        return E_INVALIDARG;
    };
    if out.is_null() {
        return E_INVALIDARG;
    }
    let queue = Queue::new([Port::new(work), Port::new(completion)]);
    // SAFETY: checked non-null; the title passes room for a handle.
    unsafe { out.write(Arc::into_raw(queue)) };
    S_OK
}

/// A port handle points at the port, which its queue keeps alive.
pub fn XTaskQueueGetPort(handle: *const Queue, port: u32, out: *mut *const c_void) -> HRESULT {
    let Some(queue) = queue(handle) else {
        return E_INVALIDARG;
    };
    if port > COMPLETION || out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null.
    unsafe { out.write(Arc::as_ptr(&queue.ports[port as usize]).cast()) };
    S_OK
}

pub fn XTaskQueueCreateComposite(
    work: *const c_void,
    completion: *const c_void,
    out: *mut *const Queue,
) -> HRESULT {
    if work.is_null() || completion.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    let port = |p: *const c_void| {
        // SAFETY: a port handle from `XTaskQueueGetPort`, whose queue is open.
        unsafe {
            Arc::increment_strong_count(p.cast::<Port>());
            Arc::from_raw(p.cast::<Port>())
        }
    };
    let queue = Queue::new([port(work), port(completion)]);
    // SAFETY: checked non-null.
    unsafe { out.write(Arc::into_raw(queue)) };
    S_OK
}

pub fn XTaskQueueDuplicateHandle(handle: *const Queue, out: *mut *const Queue) -> HRESULT {
    match queue(handle) {
        Some(queue) if !out.is_null() => {
            // SAFETY: checked non-null.
            unsafe { out.write(Arc::into_raw(queue)) };
            S_OK
        }
        _ => E_INVALIDARG,
    }
}

pub fn XTaskQueueCloseHandle(handle: *const Queue) {
    if !handle.is_null() {
        // SAFETY: a handle we returned; this drops its reference.
        unsafe { Arc::decrement_strong_count(handle) };
    }
}

/// Runs one queued callback of a manual port; false if none came in time,
/// or none can come: the queue was terminated (which ends a wait under way,
/// as a dispatcher thread's loop expects).
pub fn XTaskQueueDispatch(handle: *const Queue, port: u32, timeout_ms: u32) -> bool {
    let Some(queue) = queue(handle) else {
        return false;
    };
    let Some(port) = queue.ports.get(port as usize) else {
        return false;
    };
    if port.mode != Mode::Manual {
        return false;
    }
    match port.take(timeout_ms, || queue.terminated.load(Ordering::Acquire)) {
        Some(task) => {
            task.run(false);
            true
        }
        None => false,
    }
}

pub fn XTaskQueueSubmitDelayedCallback(
    handle: *const Queue,
    port: u32,
    delay_ms: u32,
    context: *mut c_void,
    callback: Option<Callback>,
) -> HRESULT {
    match (queue(handle), callback) {
        (Some(queue), Some(callback)) => queue.submit(port, delay_ms, Task::new(context, callback)),
        _ => E_INVALIDARG,
    }
}

/// Cancels what is still queued (callbacks see `canceled`), refuses new
/// work, then calls `callback`.
pub fn XTaskQueueTerminate(
    handle: *const Queue,
    _wait: bool,
    context: *mut c_void,
    callback: Option<TerminatedCallback>,
) -> HRESULT {
    let Some(queue) = queue(handle) else {
        return E_INVALIDARG;
    };
    queue.terminated.store(true, Ordering::Release);
    for port in &queue.ports {
        let pending: Vec<Task> = lock(&port.tasks).drain(..).collect();
        // Dispatches waiting on this queue end: nothing more will come.
        // (Those of other queues on the port see theirs is open and wait
        // on.) After the lock above, so none misses both flag and wake.
        port.ready.notify_all();
        for task in pending {
            task.run(true);
        }
    }
    if let Some(callback) = callback {
        callback(context);
    }
    S_OK
}

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

pub fn XTaskQueueRegisterMonitor(
    handle: *const Queue,
    context: *mut c_void,
    callback: Option<MonitorCallback>,
    token: *mut u64,
) -> HRESULT {
    let (Some(queue), Some(callback)) = (queue(handle), callback) else {
        return E_INVALIDARG;
    };
    if token.is_null() {
        return E_INVALIDARG;
    }
    let id = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    lock(&queue.monitors).push((id, context as usize, callback as usize));
    // SAFETY: checked non-null.
    unsafe { token.write(id) };
    S_OK
}

pub fn XTaskQueueUnregisterMonitor(handle: *const Queue, token: u64) {
    if let Some(queue) = queue(handle) {
        lock(&queue.monitors).retain(|m| m.0 != token);
    }
}

/// A new handle to the process queue, if there is one.
pub fn XTaskQueueGetCurrentProcessTaskQueue(out: *mut *const Queue) -> bool {
    let found = process_queue();
    if !out.is_null() {
        // SAFETY: checked non-null.
        unsafe { out.write(found.clone().map_or(std::ptr::null(), Arc::into_raw)) };
    }
    found.is_some()
}

pub fn XTaskQueueSetCurrentProcessTaskQueue(handle: *const Queue) {
    let new = if handle.is_null() {
        None
    } else {
        queue(handle)
    };
    *lock(&PROCESS_QUEUE) = Some(new);
}

/// An async block: what the title fills in, then the system area.
#[repr(C)]
pub struct AsyncBlock {
    pub queue: *const Queue,
    pub context: *mut c_void,
    pub callback: Option<CompletionRoutine>,
    /// Ours: [unused, status, result size, unused].
    pub internal: [usize; 4],
}

#[repr(C)]
pub struct ProviderData {
    pub block: *mut AsyncBlock,
    pub buffer_size: usize,
    pub buffer: *mut c_void,
    pub context: *mut c_void,
}

const OP_BEGIN: u32 = 0;
const OP_DO_WORK: u32 = 1;
const OP_GET_RESULT: u32 = 2;
const OP_CANCEL: u32 = 3;
const OP_CLEANUP: u32 = 4;

/// One call in flight, until its result is taken or it fails.
struct Call {
    provider: Provider,
    context: usize,
    identity: usize,
    done: Mutex<bool>,
    finished: Condvar,
    ops: Mutex<Ops>,
}

/// The provider's operations under way on a call, and whether the call is
/// over. `Cleanup` is where a provider frees its context ("data in the
/// context can be cleaned up", as the public reference has it), so it is
/// the last operation a provider sees: none starts once the call is over,
/// and the cleanup waits for those that other threads are still in.
#[derive(Default)]
struct Ops {
    /// The thread of each operation under way.
    running: Vec<std::thread::ThreadId>,
    /// The call left the table: no further operation starts.
    over: bool,
    cleaned: bool,
}

impl Ops {
    /// Whether the calling thread is to run the cleanup now: the call is
    /// over, nobody has cleaned up, and no other thread is in the
    /// provider. Operations of the calling thread itself do not hold it
    /// back: a provider that completes its call from inside one of them
    /// has asked for the cleanup there.
    fn cleanup_due(&mut self) -> bool {
        let me = std::thread::current().id();
        let due = self.over && !self.cleaned && self.running.iter().all(|t| *t == me);
        self.cleaned |= due;
        due
    }
}

impl Call {
    fn new(provider: Provider, context: *mut c_void, identity: *const c_void) -> Arc<Call> {
        Arc::new(Call {
            provider,
            context: context as usize,
            identity: identity as usize,
            done: Mutex::new(false),
            finished: Condvar::new(),
            ops: Mutex::default(),
        })
    }

    /// Calls the provider with `op` unless the call is over (`None`: the
    /// provider was not called). If the call ended on another thread
    /// meanwhile, its cleanup follows here.
    fn op(
        &self,
        op: u32,
        block: *mut AsyncBlock,
        size: usize,
        buffer: *mut c_void,
    ) -> Option<HRESULT> {
        let me = std::thread::current().id();
        {
            let mut ops = lock(&self.ops);
            if ops.over {
                return None;
            }
            ops.running.push(me);
        }
        let hr = self.provider(op, block, size, buffer);
        let cleanup = {
            let mut ops = lock(&self.ops);
            if let Some(at) = ops.running.iter().rposition(|t| *t == me) {
                ops.running.swap_remove(at);
            }
            ops.cleanup_due()
        };
        if cleanup {
            self.provider(OP_CLEANUP, block, 0, std::ptr::null_mut());
        }
        Some(hr)
    }

    /// The call has left the table: no operation starts from here on, and
    /// the provider cleans up, now or when the operations other threads
    /// are in have returned. Called without the table's lock, since the
    /// provider may call back into us.
    fn end(&self, block: *mut AsyncBlock) {
        let cleanup = {
            let mut ops = lock(&self.ops);
            ops.over = true;
            ops.cleanup_due()
        };
        if cleanup {
            self.provider(OP_CLEANUP, block, 0, std::ptr::null_mut());
        }
    }

    fn provider(
        &self,
        op: u32,
        block: *mut AsyncBlock,
        size: usize,
        buffer: *mut c_void,
    ) -> HRESULT {
        let data = ProviderData {
            block,
            buffer_size: size,
            buffer,
            context: self.context as *mut c_void,
        };
        (self.provider)(op, &data)
    }
}

/// Calls by block address.
static CALLS: LazyLock<Mutex<HashMap<usize, Arc<Call>>>> = LazyLock::new(Default::default);

fn call(block: *mut AsyncBlock) -> Option<Arc<Call>> {
    lock(&CALLS).get(&(block as usize)).cloned()
}

/// The block's status and result-size words.
fn words(block: *mut AsyncBlock) -> (&'static AtomicUsize, &'static AtomicUsize) {
    // SAFETY: the title's block outlives the call; its system area is ours.
    unsafe {
        let internal = std::ptr::addr_of_mut!((*block).internal).cast::<AtomicUsize>();
        (&*internal.add(1), &*internal.add(2))
    }
}

fn status(block: *mut AsyncBlock) -> HRESULT {
    words(block).0.load(Ordering::Acquire) as u32 as i32
}

/// Ends the call on `block` whose status is already final.
fn release(block: *mut AsyncBlock) {
    // Unlocked before the provider runs: it may call back into us.
    let removed = lock(&CALLS).remove(&(block as usize));
    if let Some(call) = removed {
        call.end(block);
    }
}

pub fn XAsyncBegin(
    block: *mut AsyncBlock,
    context: *mut c_void,
    identity: *const c_void,
    _identity_name: *const u8,
    provider: Option<Provider>,
) -> HRESULT {
    let Some(provider) = provider else {
        return E_INVALIDARG;
    };
    if block.is_null() || call(block).is_some() {
        return E_INVALIDARG;
    }
    let (status, size) = words(block);
    status.store(E_PENDING as u32 as usize, Ordering::Release);
    size.store(0, Ordering::Release);
    let call = Call::new(provider, context, identity);
    lock(&CALLS).insert(block as usize, call.clone());
    // A failed begin fails the call, so the completion routine still runs.
    if let Some(hr) = call.op(OP_BEGIN, block, 0, std::ptr::null_mut())
        && hr < 0
    {
        complete(&call, block, hr, 0);
    }
    S_OK
}

/// Queues the provider's `DoWork` on the block's work port.
pub fn XAsyncSchedule(block: *mut AsyncBlock, delay_ms: u32) -> HRESULT {
    extern "system" fn work(block: *mut c_void, canceled: bool) {
        let block = block.cast::<AsyncBlock>();
        let Some(call) = call(block) else { return };
        let hr = if canceled {
            E_ABORT
        } else {
            // A call that ended while its work was queued has none to do.
            match call.op(OP_DO_WORK, block, 0, std::ptr::null_mut()) {
                Some(hr) => hr,
                None => return,
            }
        };
        // This call, not whichever one is on the block by now: a provider
        // that completed inside `DoWork` has ended it, and the title may
        // have begun another on the same block since.
        if hr != E_PENDING {
            complete(&call, block, hr, 0);
        }
    }
    if block.is_null() || call(block).is_none() {
        return E_INVALIDARG;
    }
    // SAFETY: a block in flight is valid.
    let handle = unsafe { (*block).queue };
    match queue(handle) {
        Some(queue) => queue.submit(WORK, delay_ms, Task::new(block.cast(), work)),
        None => E_INVALIDARG,
    }
}

/// Records the result (the first one wins), wakes waiters and queues the
/// completion routine on the block's completion port. A title's provider
/// names its call by the block, which is all it has.
pub fn XAsyncComplete(block: *mut AsyncBlock, result: HRESULT, required_size: usize) {
    if let Some(call) = call(block) {
        complete(&call, block, result, required_size);
    }
}

/// [`XAsyncComplete`] of `call`, which was begun on `block`. Our own
/// completions come through here with the call they mean: one that follows
/// a provider operation may arrive when that call is over and the title
/// has begun a new one on the same block, which it must not touch. A call
/// that is over is recognised without reading the block, which the title
/// may have freed.
fn complete(call: &Arc<Call>, block: *mut AsyncBlock, result: HRESULT, required_size: usize) {
    extern "system" fn run_routine(block: *mut c_void, _canceled: bool) {
        let block = block.cast::<AsyncBlock>();
        // SAFETY: read before the status was published; see below.
        if let Some(routine) = unsafe { (*block).callback } {
            routine(block);
        }
    }
    if result == E_PENDING {
        return;
    }
    let mut done = lock(&call.done);
    if *done {
        return;
    }
    // Not done, so still the block's call; checked all the same, since
    // everything below writes to the block.
    let current = lock(&CALLS)
        .get(&(block as usize))
        .is_some_and(|c| Arc::ptr_eq(c, call));
    if !current {
        return;
    }
    // Read before publishing: once the status is set, the title may free
    // the block.
    // SAFETY: a block in flight is valid.
    let (handle, routine) = unsafe { ((*block).queue, (*block).callback) };
    let (status, size) = words(block);
    // A call with nothing to hand out ends here. It leaves the table and
    // its status becomes final in one step under the table's lock: a
    // waiter that no longer finds the call must not read a status that
    // still says pending (`XAsyncGetStatus` would return `E_PENDING` from
    // a wait), and a title that reads the final status may begin a new
    // call on the same block at once.
    let ended = {
        let mut calls = lock(&CALLS);
        size.store(required_size, Ordering::Release);
        status.store(result as u32 as usize, Ordering::Release);
        (result < 0 || required_size == 0)
            .then(|| calls.remove(&(block as usize)))
            .flatten()
    };
    // Unlocked before the provider runs: it may call back into us.
    if let Some(ended) = ended {
        ended.end(block);
    }
    *done = true;
    call.finished.notify_all();
    drop(done);
    if routine.is_some() {
        let task = Task::new(block.cast(), run_routine);
        match queue(handle) {
            Some(queue) if queue.submit(COMPLETION, 0, task) == S_OK => {}
            _ => task.run(true),
        }
    }
}

pub fn XAsyncGetStatus(block: *mut AsyncBlock, wait: bool) -> HRESULT {
    if block.is_null() {
        return E_INVALIDARG;
    }
    if wait && let Some(call) = call(block) {
        let done = lock(&call.done);
        drop(call.finished.wait_while(done, |d| !*d));
    }
    status(block)
}

pub fn XAsyncGetResultSize(block: *mut AsyncBlock, out: *mut usize) -> HRESULT {
    if block.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    let hr = status(block);
    // Any nonnegative completion carries a result, not just S_OK: S_FALSE
    // is a success code too and a provider may return data with it.
    if hr >= 0 {
        // SAFETY: checked non-null.
        unsafe { out.write(words(block).1.load(Ordering::Acquire)) };
    }
    hr
}

/// Asks the provider to cancel; the call fails with `E_ABORT` unless the
/// provider completed it first.
pub fn XAsyncCancel(block: *mut AsyncBlock) {
    if let Some(call) = call(block) {
        cancel(&call, block);
    }
}

/// [`XAsyncCancel`] of a call found in the table a moment ago. It may have
/// completed on another thread since, and its provider cleaned up: then
/// there is nothing left to cancel, and the provider is not asked.
fn cancel(call: &Arc<Call>, block: *mut AsyncBlock) {
    // The provider may have completed the call inside its cancel, or
    // another thread while it was there, and the title begun a new one on
    // the block: the `E_ABORT` is for this call only.
    if call.op(OP_CANCEL, block, 0, std::ptr::null_mut()).is_some() {
        complete(call, block, E_ABORT, 0);
    }
}

/// Has the provider write the result into `buffer`, then ends the call.
pub fn XAsyncGetResult(
    block: *mut AsyncBlock,
    identity: *const c_void,
    buffer_size: usize,
    buffer: *mut c_void,
    used: *mut usize,
) -> HRESULT {
    if block.is_null() {
        return E_INVALIDARG;
    }
    let hr = status(block);
    let size = words(block).1.load(Ordering::Acquire);
    if hr < 0 || size == 0 {
        if hr >= 0 && !used.is_null() {
            // SAFETY: checked non-null.
            unsafe { used.write(0) };
        }
        return hr;
    }
    let Some(call) = call(block) else {
        return E_ILLEGAL_METHOD_CALL;
    };
    if call.identity != identity as usize {
        return E_INVALIDARG;
    }
    if buffer_size < size || buffer.is_null() {
        return E_NOT_SUFFICIENT_BUFFER;
    }
    // Taken by another thread since the look-up: as for no call at all.
    let Some(hr) = call.op(OP_GET_RESULT, block, buffer_size, buffer) else {
        return E_ILLEGAL_METHOD_CALL;
    };
    if !used.is_null() {
        // SAFETY: checked non-null.
        unsafe { used.write(size) };
    }
    release(block);
    hr
}

/// Starts a call on `block` that is already over: it completes with `hr`
/// and no result data, through the block's completion port as usual.
pub fn finished(block: *mut AsyncBlock, identity: *const c_void, hr: HRESULT) -> HRESULT {
    finished_with(
        block,
        identity,
        if hr < 0 { Err(hr) } else { Ok(Vec::new()) },
    )
}

/// Like [`finished`], with the bytes `XAsyncGetResult` hands out for
/// `identity` (an error has none).
pub fn finished_with(
    block: *mut AsyncBlock,
    identity: *const c_void,
    result: Result<Vec<u8>, HRESULT>,
) -> HRESULT {
    type Data = Result<Vec<u8>, HRESULT>;
    extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
        // SAFETY: the runtime passes valid provider data; the context is the
        // boxed result until cleanup frees it.
        let data = unsafe { &*data };
        let result = data.context.cast::<Data>();
        match op {
            // SAFETY: boxed in `finished_with`; nothing runs after cleanup.
            OP_CLEANUP => drop(unsafe { Box::from_raw(result) }),
            OP_BEGIN => {
                // SAFETY: as above. Copied out: completing may clean up.
                let (hr, len) = match unsafe { &*result } {
                    Ok(bytes) => (S_OK, bytes.len()),
                    Err(hr) => (*hr, 0),
                };
                XAsyncComplete(data.block, hr, len);
            }
            // SAFETY: as above; the runtime checked the buffer's size.
            OP_GET_RESULT => unsafe {
                if let Ok(bytes) = &*result {
                    data.buffer
                        .cast::<u8>()
                        .copy_from(bytes.as_ptr(), bytes.len());
                }
            },
            _ => {}
        }
        S_OK
    }
    let context = Box::into_raw(Box::new(result));
    let hr = XAsyncBegin(
        block,
        context.cast(),
        identity,
        std::ptr::null(),
        Some(provider),
    );
    if hr < 0 {
        // Refused before the call existed, so no cleanup will come.
        // SAFETY: still ours.
        drop(unsafe { Box::from_raw(context) });
    }
    hr
}

/// Like [`finished_with`], but the result comes from `work`, run on a
/// thread of ours; the call stays pending until it returns (a dialog, say).
pub fn pending_with(
    block: *mut AsyncBlock,
    identity: *const c_void,
    work: impl FnOnce() -> Result<Vec<u8>, HRESULT> + Send + 'static,
) -> HRESULT {
    type Data = Mutex<(
        Option<Box<dyn FnOnce() -> Result<Vec<u8>, HRESULT> + Send>>,
        Vec<u8>,
    )>;
    extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
        // SAFETY: the runtime passes valid provider data; the context is a
        // reference to the shared state until cleanup gives it up.
        let data = unsafe { &*data };
        let state = data.context.cast::<Data>().cast_const();
        match op {
            // SAFETY: the reference made in `pending_with`; nothing runs
            // after cleanup.
            OP_CLEANUP => drop(unsafe { Arc::from_raw(state) }),
            OP_BEGIN => {
                // The worker shares the state and names its call: a cancel
                // ends the call and cleans up while the work is still under
                // way, and the title may begin a new call on the block
                // before the work returns.
                // SAFETY: as above; a second reference for the worker.
                let state = unsafe {
                    Arc::increment_strong_count(state);
                    Arc::from_raw(state)
                };
                let work = lock(&state).0.take();
                // `XAsyncBegin` is still in this call, so it is the block's.
                let call = call(data.block);
                let block = data.block as usize;
                std::thread::spawn(move || {
                    let result = work.map_or(Err(E_ABORT), |work| work());
                    let (hr, len) = match &result {
                        Ok(bytes) => (S_OK, bytes.len()),
                        Err(hr) => (*hr, 0),
                    };
                    if let Ok(bytes) = result {
                        lock(&state).1 = bytes;
                    }
                    if let Some(call) = call {
                        complete(&call, block as *mut AsyncBlock, hr, len);
                        #[cfg(test)]
                        lock(&WORKERS_RETURNED).push(call.identity);
                    }
                });
            }
            // SAFETY: as above; the runtime checked the buffer's size.
            OP_GET_RESULT => unsafe {
                let state = lock(&*state);
                data.buffer
                    .cast::<u8>()
                    .copy_from(state.1.as_ptr(), state.1.len());
            },
            _ => {}
        }
        S_OK
    }
    let state: Arc<Data> = Arc::new(Mutex::new((Some(Box::new(work)), Vec::new())));
    let context = Arc::into_raw(state).cast_mut();
    let hr = XAsyncBegin(
        block,
        context.cast(),
        identity,
        std::ptr::null(),
        Some(provider),
    );
    if hr < 0 {
        // SAFETY: refused before the call existed; still ours.
        drop(unsafe { Arc::from_raw(context) });
    }
    hr
}

/// The identities of the calls whose [`pending_with`] worker has returned
/// and made its completion, for tests that must wait for one.
#[cfg(test)]
static WORKERS_RETURNED: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// The result of a call begun with [`finished_with`] for `identity`, as a
/// `T` written through `out`.
pub fn result<T>(block: *mut AsyncBlock, identity: *const c_void, out: *mut T) -> HRESULT {
    XAsyncGetResult(
        block,
        identity,
        size_of::<T>(),
        out.cast(),
        std::ptr::null_mut(),
    )
}

/// The bytes of a value, for [`finished_with`].
pub fn bytes<T: Copy>(value: T) -> Vec<u8> {
    // SAFETY: a `Copy` value is plain data; padding bytes are never read.
    unsafe { std::slice::from_raw_parts((&raw const value).cast::<u8>(), size_of::<T>()) }.to_vec()
}

type Work = extern "system" fn(*mut AsyncBlock) -> HRESULT;

/// Runs `work` on the block's work port as an async call of its own; its
/// return value is the call's result.
pub fn XAsyncRun(block: *mut AsyncBlock, work: Option<Work>) -> HRESULT {
    extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
        // SAFETY: the runtime passes valid provider data.
        let data = unsafe { &*data };
        match op {
            OP_BEGIN => XAsyncSchedule(data.block, 0),
            OP_DO_WORK => {
                // SAFETY: the context is the `Work` given to `XAsyncRun`.
                let work = unsafe { std::mem::transmute::<*mut c_void, Work>(data.context) };
                work(data.block)
            }
            _ => S_OK,
        }
    }
    let Some(work) = work else {
        return E_INVALIDARG;
    };
    let identity = XAsyncRun as *const c_void;
    XAsyncBegin(
        block,
        work as *mut c_void,
        identity,
        std::ptr::null(),
        Some(provider),
    )
}

// The object `QueryApiImpl` returns. Each method takes the object first,
// then the API's arguments; slot offsets as titles call them.

pub static OBJECT: Object = Object::new("task queue", &VTABLE);

/// Its interface id is the provider's.
pub const INTERFACES: &[Interface] = &[(PROVIDER, PROVIDER, &OBJECT)];

type This = *const Object;
extern "system" fn m_get_status(_: This, b: *mut AsyncBlock, wait: u8) -> HRESULT {
    XAsyncGetStatus(b, wait != 0)
}
extern "system" fn m_get_result_size(_: This, b: *mut AsyncBlock, out: *mut usize) -> HRESULT {
    XAsyncGetResultSize(b, out)
}
extern "system" fn m_cancel(_: This, b: *mut AsyncBlock) {
    XAsyncCancel(b)
}
extern "system" fn m_run(_: This, b: *mut AsyncBlock, work: Option<Work>) -> HRESULT {
    XAsyncRun(b, work)
}
extern "system" fn m_begin(
    _: This,
    b: *mut AsyncBlock,
    context: *mut c_void,
    identity: *const c_void,
    name: *const u8,
    provider: Option<Provider>,
) -> HRESULT {
    XAsyncBegin(b, context, identity, name, provider)
}
extern "system" fn m_schedule(_: This, b: *mut AsyncBlock, delay: u32) -> HRESULT {
    XAsyncSchedule(b, delay)
}
extern "system" fn m_complete(_: This, b: *mut AsyncBlock, hr: HRESULT, size: usize) {
    XAsyncComplete(b, hr, size)
}
extern "system" fn m_get_result(
    _: This,
    b: *mut AsyncBlock,
    identity: *const c_void,
    size: usize,
    buffer: *mut c_void,
    used: *mut usize,
) -> HRESULT {
    XAsyncGetResult(b, identity, size, buffer, used)
}
extern "system" fn m_create(
    _: This,
    work: u32,
    completion: u32,
    out: *mut *const Queue,
) -> HRESULT {
    XTaskQueueCreate(work, completion, out)
}
extern "system" fn m_create_composite(
    _: This,
    work: *const c_void,
    completion: *const c_void,
    out: *mut *const Queue,
) -> HRESULT {
    XTaskQueueCreateComposite(work, completion, out)
}
extern "system" fn m_get_port(
    _: This,
    q: *const Queue,
    port: u32,
    out: *mut *const c_void,
) -> HRESULT {
    XTaskQueueGetPort(q, port, out)
}
extern "system" fn m_duplicate(_: This, q: *const Queue, out: *mut *const Queue) -> HRESULT {
    XTaskQueueDuplicateHandle(q, out)
}
extern "system" fn m_dispatch(_: This, q: *const Queue, port: u32, timeout: u32) -> bool {
    XTaskQueueDispatch(q, port, timeout)
}
extern "system" fn m_close(_: This, q: *const Queue) {
    XTaskQueueCloseHandle(q)
}
extern "system" fn m_submit(
    _: This,
    q: *const Queue,
    port: u32,
    context: *mut c_void,
    callback: Option<Callback>,
) -> HRESULT {
    XTaskQueueSubmitDelayedCallback(q, port, 0, context, callback)
}
extern "system" fn m_submit_delayed(
    _: This,
    q: *const Queue,
    port: u32,
    delay: u32,
    context: *mut c_void,
    callback: Option<Callback>,
) -> HRESULT {
    XTaskQueueSubmitDelayedCallback(q, port, delay, context, callback)
}
extern "system" fn m_terminate(
    _: This,
    q: *const Queue,
    wait: u8,
    context: *mut c_void,
    callback: Option<TerminatedCallback>,
) -> HRESULT {
    XTaskQueueTerminate(q, wait != 0, context, callback)
}
extern "system" fn m_register_monitor(
    _: This,
    q: *const Queue,
    context: *mut c_void,
    callback: Option<MonitorCallback>,
    token: *mut u64,
) -> HRESULT {
    XTaskQueueRegisterMonitor(q, context, callback, token)
}
extern "system" fn m_unregister_monitor(_: This, q: *const Queue, token: u64) {
    XTaskQueueUnregisterMonitor(q, token)
}
extern "system" fn m_get_process_queue(_: This, out: *mut *const Queue) -> bool {
    XTaskQueueGetCurrentProcessTaskQueue(out)
}
extern "system" fn m_set_process_queue(_: This, q: *const Queue) {
    XTaskQueueSetCurrentProcessTaskQueue(q)
}
/// The runtime's debug check that the caller is not a time-critical thread,
/// which every runtime call in a title makes first.
extern "system" fn m_check_thread(_: This) {}

static VTABLE: Methods<31> = Methods([
    missing::<0> as *const (),         // 0x00 QueryInterface
    add_ref as *const (),              // 0x08 AddRef
    add_ref as *const (),              // 0x10 Release: the object is static
    m_get_status as *const (),         // 0x18
    m_get_result_size as *const (),    // 0x20
    m_cancel as *const (),             // 0x28
    m_run as *const (),                // 0x30
    m_begin as *const (),              // 0x38
    missing::<8> as *const (),         // 0x40
    m_schedule as *const (),           // 0x48
    m_complete as *const (),           // 0x50
    m_get_result as *const (),         // 0x58
    m_create as *const (),             // 0x60
    m_create_composite as *const (),   // 0x68
    m_get_port as *const (),           // 0x70
    m_duplicate as *const (),          // 0x78
    m_dispatch as *const (),           // 0x80
    m_close as *const (),              // 0x88
    m_submit as *const (),             // 0x90
    m_submit_delayed as *const (),     // 0x98
    missing::<20> as *const (),        // 0xa0 register a wait handle
    missing::<21> as *const (),        // 0xa8 unregister it
    m_terminate as *const (),          // 0xb0
    m_register_monitor as *const (),   // 0xb8
    m_unregister_monitor as *const (), // 0xc0
    m_get_process_queue as *const (),  // 0xc8
    m_set_process_queue as *const (),  // 0xd0
    missing::<27> as *const (),        // 0xd8 thread time-sensitivity
    missing::<28> as *const (),        // 0xe0
    m_check_thread as *const (),       // 0xe8
    missing::<30> as *const (),        // 0xf0
]);

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    fn slot<F: Copy>(offset: usize) -> F {
        crate::modules::xgameruntime::method(&OBJECT, offset)
    }

    static RAN: AtomicU32 = AtomicU32::new(0);
    extern "system" fn count(context: *mut c_void, canceled: bool) {
        RAN.fetch_add(
            if canceled { 100 } else { context as u32 },
            Ordering::SeqCst,
        );
    }

    #[test]
    fn manual_ports_run_callbacks_when_dispatched() {
        type Create = extern "system" fn(This, u32, u32, *mut *const Queue) -> HRESULT;
        type Submit = extern "system" fn(This, *const Queue, u32, *mut c_void, Callback) -> HRESULT;
        type Dispatch = extern "system" fn(This, *const Queue, u32, u32) -> bool;
        type Close = extern "system" fn(This, *const Queue);
        let this: This = &OBJECT;
        let mut q = std::ptr::null();
        assert_eq!(slot::<Create>(0x60)(this, 0, 0, &mut q), S_OK);
        assert_eq!(
            slot::<Submit>(0x90)(this, q, COMPLETION, 5 as *mut c_void, count),
            S_OK
        );
        assert_eq!(RAN.load(Ordering::SeqCst), 0, "manual: nothing runs yet");
        assert!(
            !slot::<Dispatch>(0x80)(this, q, WORK, 0),
            "work port is empty"
        );
        assert!(slot::<Dispatch>(0x80)(this, q, COMPLETION, 0));
        assert_eq!(RAN.load(Ordering::SeqCst), 5);
        assert!(!slot::<Dispatch>(0x80)(this, q, COMPLETION, 0));
        slot::<Close>(0x88)(this, q);
    }

    /// A provider that schedules its work and returns a u64 result.
    extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
        // SAFETY: the runtime passes valid provider data.
        let data = unsafe { &*data };
        match op {
            OP_BEGIN => XAsyncSchedule(data.block, 0),
            OP_DO_WORK => {
                XAsyncComplete(data.block, S_OK, 8);
                S_OK
            }
            OP_GET_RESULT => {
                // SAFETY: at least 8 bytes, checked by the runtime.
                unsafe { data.buffer.cast::<u64>().write(data.context as u64) };
                S_OK
            }
            _ => S_OK,
        }
    }

    static COMPLETED: AtomicU32 = AtomicU32::new(0);
    extern "system" fn completed(_: *mut AsyncBlock) {
        COMPLETED.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn async_calls_complete_and_return_results() {
        let mut q = std::ptr::null();
        // Work on a thread pool, completion dispatched by hand.
        assert_eq!(XTaskQueueCreate(1, 0, &mut q), S_OK);
        let mut block = AsyncBlock {
            queue: q,
            context: std::ptr::null_mut(),
            callback: Some(completed),
            internal: [0; 4],
        };
        let identity = 0x1d as *const c_void;
        assert_eq!(
            XAsyncBegin(
                &mut block,
                42 as *mut c_void,
                identity,
                std::ptr::null(),
                Some(provider)
            ),
            S_OK
        );
        assert_eq!(XAsyncGetStatus(&mut block, true), S_OK);
        let mut size = 0;
        assert_eq!(XAsyncGetResultSize(&mut block, &mut size), S_OK);
        assert_eq!(size, 8);
        assert_eq!(COMPLETED.load(Ordering::SeqCst), 0, "completion is manual");
        assert!(XTaskQueueDispatch(q, COMPLETION, INFINITE));
        assert_eq!(COMPLETED.load(Ordering::SeqCst), 1);

        let mut value = 0u64;
        let mut small = 0u32;
        let out = (&mut small as *mut u32).cast();
        assert_eq!(
            XAsyncGetResult(&mut block, identity, 4, out, std::ptr::null_mut()),
            E_NOT_SUFFICIENT_BUFFER
        );
        let out = (&mut value as *mut u64).cast();
        let mut used = 0;
        assert_eq!(
            XAsyncGetResult(&mut block, identity, 8, out, &mut used),
            S_OK
        );
        assert_eq!((value, used), (42, 8));
        assert!(call(&mut block).is_none(), "the call ends with its result");
        XTaskQueueCloseHandle(q);
    }

    #[test]
    fn a_nonnegative_completion_still_returns_its_payload() {
        const S_FALSE: HRESULT = 1;
        extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
            // SAFETY: the runtime passes valid provider data.
            let data = unsafe { &*data };
            match op {
                OP_BEGIN => XAsyncSchedule(data.block, 0),
                OP_DO_WORK => {
                    XAsyncComplete(data.block, S_FALSE, 8);
                    S_OK
                }
                OP_GET_RESULT => {
                    // SAFETY: the caller checked eight bytes.
                    unsafe { data.buffer.cast::<u64>().write(0x1234) };
                    S_OK
                }
                _ => S_OK,
            }
        }
        let mut q = std::ptr::null();
        assert_eq!(XTaskQueueCreate(0, 0, &mut q), S_OK);
        let mut block = AsyncBlock {
            queue: q,
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        };
        let identity = 0x77 as *const c_void;
        assert_eq!(
            XAsyncBegin(
                &mut block,
                std::ptr::null_mut(),
                identity,
                std::ptr::null(),
                Some(provider)
            ),
            S_OK
        );
        assert!(XTaskQueueDispatch(q, WORK, 0));
        assert_eq!(XAsyncGetStatus(&mut block, false), S_FALSE);
        let mut size = 0;
        assert_eq!(XAsyncGetResultSize(&mut block, &mut size), S_FALSE);
        assert_eq!(size, 8);
        let mut value = 0u64;
        let mut used = 0;
        assert_eq!(
            XAsyncGetResult(
                &mut block,
                identity,
                8,
                (&mut value as *mut u64).cast(),
                &mut used
            ),
            S_OK
        );
        assert_eq!((value, used), (0x1234, 8));
        assert!(
            call(&raw mut block).is_none(),
            "the call ends with its result"
        );
        XTaskQueueCloseHandle(q);
    }

    /// A failed call leaves the table before its provider cleans up. A
    /// wait that starts in between finds no call to wait on, so the status
    /// must be final by then: it once still said pending, and a title
    /// waiting for a cancelled dialog got `E_PENDING` back.
    #[test]
    fn a_wait_during_cleanup_sees_the_final_status() {
        use std::sync::mpsc::{Receiver, Sender, channel};
        struct Gate {
            cleaning: Mutex<Sender<()>>,
            go_on: Mutex<Receiver<()>>,
        }
        extern "system" fn provider(op: u32, data: *const ProviderData) -> HRESULT {
            if op == OP_CLEANUP {
                // SAFETY: the context is the test's gate, which outlives
                // the call.
                let gate = unsafe { &*(*data).context.cast::<Gate>() };
                let _ = lock(&gate.cleaning).send(());
                let _ = lock(&gate.go_on).recv_timeout(Duration::from_secs(30));
            }
            S_OK
        }
        let (cleaning, in_cleanup) = channel();
        let (release, go_on) = channel();
        let gate = Gate {
            cleaning: Mutex::new(cleaning),
            go_on: Mutex::new(go_on),
        };
        let mut block = AsyncBlock {
            queue: std::ptr::null(),
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        };
        let context = (&raw const gate).cast_mut().cast();
        assert_eq!(
            XAsyncBegin(
                &mut block,
                context,
                std::ptr::null(),
                std::ptr::null(),
                Some(provider)
            ),
            S_OK
        );
        assert_eq!(XAsyncGetStatus(&mut block, false), E_PENDING);
        let address = &raw mut block as usize;
        let completing = std::thread::spawn(move || {
            XAsyncComplete(address as *mut AsyncBlock, E_ABORT, 0);
        });
        // The completing thread now stands in the provider's cleanup.
        assert!(in_cleanup.recv_timeout(Duration::from_secs(30)).is_ok());
        assert!(call(&raw mut block).is_none(), "the call has ended");
        let waited = XAsyncGetStatus(&mut block, true);
        let polled = XAsyncGetStatus(&mut block, false);
        let mut size = 1;
        let sized = XAsyncGetResultSize(&mut block, &mut size);
        release.send(()).unwrap();
        completing.join().unwrap();
        assert_eq!((waited, polled, sized), (E_ABORT, E_ABORT, E_ABORT));
        assert_eq!(XAsyncGetStatus(&mut block, true), E_ABORT);
    }

    /// What a provider was asked to do, in order, and a way to hold it in
    /// its cancel operation.
    #[derive(Default)]
    struct Seen {
        ops: Mutex<Vec<&'static str>>,
        /// Sent to when the provider is in `Cancel`; it then waits for
        /// the second channel.
        park: Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
    }

    impl Seen {
        fn ops(&self) -> Vec<&'static str> {
            lock(&self.ops).clone()
        }
    }

    /// Notes every operation; completes with a failure when it works.
    extern "system" fn noting(op: u32, data: *const ProviderData) -> HRESULT {
        // SAFETY: the context is the test's `Seen`, which outlives the
        // call.
        let (seen, block) = unsafe { (&*(*data).context.cast::<Seen>(), (*data).block) };
        let note = |what| lock(&seen.ops).push(what);
        match op {
            OP_BEGIN => note("begin"),
            OP_DO_WORK => {
                note("work");
                XAsyncComplete(block, E_NOTIMPL, 0);
                note("work done");
            }
            OP_GET_RESULT => note("result"),
            OP_CANCEL => {
                note("cancel");
                if let Some((parked, go_on)) = lock(&seen.park).take() {
                    let _ = parked.send(());
                    let _ = go_on.recv_timeout(Duration::from_secs(30));
                }
                note("cancel done");
            }
            OP_CLEANUP => note("cleanup"),
            _ => note("unknown"),
        }
        S_OK
    }

    fn begin_noting(block: &mut AsyncBlock, seen: &Seen) {
        let context = std::ptr::from_ref(seen).cast_mut().cast();
        let (identity, name) = (std::ptr::null(), std::ptr::null());
        assert_eq!(
            XAsyncBegin(block, context, identity, name, Some(noting)),
            S_OK
        );
        assert_eq!(XAsyncGetStatus(block, false), E_PENDING);
    }

    fn manual_block(queue: *const Queue) -> AsyncBlock {
        AsyncBlock {
            queue,
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        }
    }

    /// `Cleanup` is where a provider frees its context, so no operation
    /// may follow it. A cancel that found the call just before another
    /// thread completed it and cleaned up does not reach the provider any
    /// more, and does not turn the call's result into `E_ABORT`; nor does
    /// work still queued for the call.
    #[test]
    fn a_call_that_is_over_is_not_cancelled() {
        let mut q = std::ptr::null();
        assert_eq!(XTaskQueueCreate(0, 0, &mut q), S_OK);
        let seen = Seen::default();
        let mut block = manual_block(q);
        begin_noting(&mut block, &seen);
        assert_eq!(XAsyncSchedule(&mut block, 0), S_OK);
        // Where `XAsyncCancel` stands once it has looked the call up.
        let found = call(&mut block).expect("a call in flight");

        XAsyncComplete(&mut block, E_INVALIDARG, 0);
        assert_eq!(seen.ops(), ["begin", "cleanup"]);

        cancel(&found, &mut block);
        assert_eq!(
            found.op(OP_CANCEL, &mut block, 0, std::ptr::null_mut()),
            None
        );
        assert_eq!(
            found.op(OP_DO_WORK, &mut block, 0, std::ptr::null_mut()),
            None
        );
        assert_eq!(
            found.op(OP_GET_RESULT, &mut block, 0, std::ptr::null_mut()),
            None
        );
        XAsyncCancel(&mut block);
        // The work queued before the call ended finds no call.
        assert!(XTaskQueueDispatch(q, WORK, 0));
        assert_eq!(seen.ops(), ["begin", "cleanup"]);
        assert_eq!(XAsyncGetStatus(&mut block, true), E_INVALIDARG);
        XTaskQueueCloseHandle(q);
    }

    /// A call completed while another thread is in the provider's cancel
    /// is over at once for the title (its status is final), but the
    /// provider cleans up only when the cancel has returned.
    #[test]
    fn a_cancel_under_way_holds_back_the_cleanup() {
        use std::sync::mpsc::channel;
        let (parked, in_cancel) = channel();
        let (release, go_on) = channel();
        let seen = Seen::default();
        *lock(&seen.park) = Some((parked, go_on));
        let mut block = manual_block(std::ptr::null());
        begin_noting(&mut block, &seen);

        let address = &raw mut block as usize;
        let cancelling = std::thread::spawn(move || XAsyncCancel(address as *mut AsyncBlock));
        assert!(in_cancel.recv_timeout(Duration::from_secs(30)).is_ok());
        XAsyncComplete(&mut block, E_INVALIDARG, 0);
        let during = seen.ops();
        let status = XAsyncGetStatus(&mut block, true);
        let gone = call(&mut block).is_none();
        release.send(()).unwrap();
        cancelling.join().unwrap();

        assert_eq!(during, ["begin", "cancel"]);
        assert_eq!(seen.ops(), ["begin", "cancel", "cancel done", "cleanup"]);
        // The completion won; the cancel's own `E_ABORT` came too late.
        assert!(gone, "the call has ended");
        assert_eq!(status, E_INVALIDARG);
        assert_eq!(XAsyncGetStatus(&mut block, true), E_INVALIDARG);
    }

    /// A provider that completes its call from inside one of its own
    /// operations is cleaned up there, as before: only operations on
    /// other threads hold the cleanup back.
    #[test]
    fn completing_inside_an_operation_cleans_up_at_once() {
        let mut q = std::ptr::null();
        assert_eq!(XTaskQueueCreate(0, 0, &mut q), S_OK);
        let seen = Seen::default();
        let mut block = manual_block(q);
        begin_noting(&mut block, &seen);
        assert_eq!(XAsyncSchedule(&mut block, 0), S_OK);
        assert!(XTaskQueueDispatch(q, WORK, 0));
        assert_eq!(seen.ops(), ["begin", "work", "cleanup", "work done"]);
        assert_eq!(XAsyncGetStatus(&mut block, true), E_NOTIMPL);
        XTaskQueueCloseHandle(q);
    }

    /// A provider that ends its call inside `DoWork` or `Cancel`, where
    /// the title, seeing the final status, at once begins the next call on
    /// the same block.
    struct Reused {
        next: Seen,
    }

    extern "system" fn reusing(op: u32, data: *const ProviderData) -> HRESULT {
        // SAFETY: the context is the test's `Reused`, which outlives both
        // calls; the block is the test's.
        let (reused, block) = unsafe { (&*(*data).context.cast::<Reused>(), (*data).block) };
        if op == OP_DO_WORK || op == OP_CANCEL {
            XAsyncComplete(block, E_NOTIMPL, 0);
            // SAFETY: as above.
            begin_noting(unsafe { &mut *block }, &reused.next);
        }
        S_OK
    }

    fn begin_reusing(block: &mut AsyncBlock, reused: &Reused) {
        let context = std::ptr::from_ref(reused).cast_mut().cast();
        let (identity, name) = (std::ptr::null(), std::ptr::null());
        assert_eq!(
            XAsyncBegin(block, context, identity, name, Some(reusing)),
            S_OK
        );
    }

    /// The next call on the block is untouched: pending, its provider not
    /// cleaned up. Then ends it.
    fn assert_next_call_is_pending(block: &mut AsyncBlock, next: &Seen) {
        let status = XAsyncGetStatus(block, false);
        let mut size = 7;
        let sized = XAsyncGetResultSize(block, &mut size);
        assert_eq!((status, sized, size), (E_PENDING, E_PENDING, 7));
        assert_eq!(next.ops(), ["begin"]);
        XAsyncComplete(block, E_INVALIDARG, 0);
        assert_eq!(XAsyncGetStatus(block, false), E_INVALIDARG);
        assert_eq!(next.ops(), ["begin", "cleanup"]);
    }

    /// A provider that completes inside `DoWork` and returns anything but
    /// `E_PENDING` has its return value completed too, as the call's
    /// result if it had none. That second completion is for the call the
    /// work belonged to: it once went to whichever call was on the block
    /// by then, and ended the title's next call with size 0.
    #[test]
    fn work_that_returns_late_does_not_complete_the_next_call() {
        let mut q = std::ptr::null();
        assert_eq!(XTaskQueueCreate(0, 0, &mut q), S_OK);
        let reused = Reused {
            next: Seen::default(),
        };
        let mut block = manual_block(q);
        begin_reusing(&mut block, &reused);
        assert_eq!(XAsyncSchedule(&mut block, 0), S_OK);
        assert!(XTaskQueueDispatch(q, WORK, 0));
        assert_next_call_is_pending(&mut block, &reused.next);
        XTaskQueueCloseHandle(q);
    }

    /// The same for the `E_ABORT` that follows the provider's `Cancel`.
    #[test]
    fn a_cancel_that_returns_late_does_not_abort_the_next_call() {
        let reused = Reused {
            next: Seen::default(),
        };
        let mut block = manual_block(std::ptr::null());
        begin_reusing(&mut block, &reused);
        XAsyncCancel(&mut block);
        assert_next_call_is_pending(&mut block, &reused.next);
    }

    /// And for the worker of `pending_with` (a dialog, say): the title
    /// cancels the call, which ends it and frees the provider's context,
    /// and begins the next one before the work returns. The worker's
    /// result then belongs to no call.
    #[test]
    fn a_worker_that_returns_late_does_not_complete_the_next_call() {
        static IDENTITY: u8 = 0;
        let identity = (&raw const IDENTITY).cast::<c_void>();
        let (release, go_on) = std::sync::mpsc::channel::<()>();
        let mut block = manual_block(std::ptr::null());
        let work = move || {
            let _ = go_on.recv_timeout(Duration::from_secs(30));
            Ok(vec![1, 2, 3, 4])
        };
        assert_eq!(pending_with(&mut block, identity, work), S_OK);
        assert_eq!(XAsyncGetStatus(&mut block, false), E_PENDING);
        XAsyncCancel(&mut block);
        assert_eq!(XAsyncGetStatus(&mut block, false), E_ABORT);

        let next = Seen::default();
        begin_noting(&mut block, &next);
        release.send(()).unwrap();
        let returned = || lock(&WORKERS_RETURNED).contains(&(identity as usize));
        let start = std::time::Instant::now();
        while !returned() && start.elapsed() < Duration::from_secs(30) {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(returned(), "the worker made its completion");
        assert_next_call_is_pending(&mut block, &next);
    }

    /// A `pending_with` call that runs its course hands out the worker's
    /// bytes.
    #[test]
    fn a_worker_result_is_handed_out() {
        static IDENTITY: u8 = 0;
        let identity = (&raw const IDENTITY).cast::<c_void>();
        let mut block = manual_block(std::ptr::null());
        let work = || Ok(vec![9, 8, 7, 6]);
        assert_eq!(pending_with(&mut block, identity, work), S_OK);
        assert_eq!(XAsyncGetStatus(&mut block, true), S_OK);
        let mut value = [0u8; 4];
        assert_eq!(result(&mut block, identity, &mut value), S_OK);
        assert_eq!(value, [9, 8, 7, 6]);
        assert!(call(&mut block).is_none(), "the call ends with its result");
    }

    #[test]
    fn run_returns_the_work_result() {
        extern "system" fn work(_: *mut AsyncBlock) -> HRESULT {
            E_ABORT
        }
        let mut block = AsyncBlock {
            queue: std::ptr::null(),
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        };
        assert_eq!(
            slot::<extern "system" fn(This, *mut AsyncBlock, Option<Work>) -> HRESULT>(0x30)(
                &OBJECT,
                &mut block,
                Some(work)
            ),
            S_OK
        );
        assert_eq!(XAsyncGetStatus(&mut block, true), E_ABORT);
    }

    #[test]
    fn pool_callbacks_may_wait_for_each_other() {
        static SECOND: AtomicBool = AtomicBool::new(false);
        static FIRST: AtomicBool = AtomicBool::new(false);
        extern "system" fn waits(_: *mut c_void, _: bool) {
            let start = std::time::Instant::now();
            while !SECOND.load(Ordering::SeqCst) && start.elapsed().as_secs() < 5 {
                std::thread::yield_now();
            }
            FIRST.store(SECOND.load(Ordering::SeqCst), Ordering::SeqCst);
        }
        extern "system" fn releases(_: *mut c_void, _: bool) {
            SECOND.store(true, Ordering::SeqCst);
        }
        let mut q = std::ptr::null();
        assert_eq!(XTaskQueueCreate(1, 1, &mut q), S_OK);
        let none = std::ptr::null_mut();
        assert_eq!(
            XTaskQueueSubmitDelayedCallback(q, WORK, 0, none, Some(waits)),
            S_OK
        );
        assert_eq!(
            XTaskQueueSubmitDelayedCallback(q, WORK, 0, none, Some(releases)),
            S_OK
        );
        let start = std::time::Instant::now();
        while !FIRST.load(Ordering::SeqCst) && start.elapsed().as_secs() < 10 {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(FIRST.load(Ordering::SeqCst));
        XTaskQueueCloseHandle(q);
    }

    /// A dispatcher thread's loop, `while (XTaskQueueDispatch(queue, port,
    /// INFINITE));`, ends when its queue is terminated: nothing can be
    /// submitted to it any more, so nothing else would ever wake it.
    #[test]
    fn terminate_ends_a_waiting_dispatch() {
        static RAN: AtomicU32 = AtomicU32::new(0);
        extern "system" fn ran(_: *mut c_void, _: bool) {
            RAN.fetch_add(1, Ordering::SeqCst);
        }
        /// Dispatches `queue`'s work until a dispatch returns false, then
        /// says so.
        fn dispatcher(queue: *const Queue) -> std::sync::mpsc::Receiver<()> {
            let (done, ended) = std::sync::mpsc::channel();
            let queue = queue as usize;
            std::thread::spawn(move || {
                while XTaskQueueDispatch(queue as *const Queue, WORK, INFINITE) {}
                let _ = done.send(());
            });
            ended
        }
        let run_one = |queue, total| {
            let none = std::ptr::null_mut();
            assert_eq!(
                XTaskQueueSubmitDelayedCallback(queue, WORK, 0, none, Some(ran)),
                S_OK
            );
            for _ in 0..500 {
                if RAN.load(Ordering::SeqCst) == total {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("callback {total} did not run");
        };
        let (long, short) = (Duration::from_secs(5), Duration::from_millis(200));
        let none = std::ptr::null_mut();

        // Two queues on the same manual ports.
        let (mut a, mut b) = (std::ptr::null(), std::ptr::null());
        assert_eq!(XTaskQueueCreate(0, 0, &mut a), S_OK);
        let (mut work, mut completion) = (std::ptr::null(), std::ptr::null());
        assert_eq!(XTaskQueueGetPort(a, WORK, &mut work), S_OK);
        assert_eq!(XTaskQueueGetPort(a, COMPLETION, &mut completion), S_OK);
        assert_eq!(XTaskQueueCreateComposite(work, completion, &mut b), S_OK);

        let ended = dispatcher(a);
        run_one(a, 1);
        // The other queue's end is not this one's: the wait goes on and
        // still runs what comes in.
        assert_eq!(XTaskQueueTerminate(b, false, none, None), S_OK);
        assert!(
            ended.recv_timeout(short).is_err(),
            "ended with another queue"
        );
        run_one(a, 2);
        // Its own queue's end is.
        assert_eq!(XTaskQueueTerminate(a, false, none, None), S_OK);
        assert!(
            ended.recv_timeout(long).is_ok(),
            "still waiting on a terminated queue"
        );
        // And a dispatch that starts after the end returns at once.
        assert!(!XTaskQueueDispatch(a, WORK, INFINITE));
        XTaskQueueCloseHandle(a);
        XTaskQueueCloseHandle(b);
    }

    #[test]
    fn terminate_cancels_what_is_queued() {
        static TERMINATED: AtomicBool = AtomicBool::new(false);
        extern "system" fn terminated(_: *mut c_void) {
            TERMINATED.store(true, Ordering::SeqCst);
        }
        extern "system" fn note(context: *mut c_void, canceled: bool) {
            // SAFETY: the test passes a pointer to its flag.
            unsafe { *context.cast::<bool>() = canceled };
        }
        let mut q = std::ptr::null();
        assert_eq!(XTaskQueueCreate(0, 0, &mut q), S_OK);
        let mut canceled = false;
        let flag = (&mut canceled as *mut bool).cast();
        assert_eq!(
            XTaskQueueSubmitDelayedCallback(q, WORK, 0, flag, Some(note)),
            S_OK
        );
        assert_eq!(
            XTaskQueueTerminate(q, true, std::ptr::null_mut(), Some(terminated)),
            S_OK
        );
        assert!(canceled && TERMINATED.load(Ordering::SeqCst));
        assert_eq!(
            XTaskQueueSubmitDelayedCallback(q, WORK, 0, flag, Some(note)),
            E_ABORT
        );
        XTaskQueueCloseHandle(q);
    }
}
