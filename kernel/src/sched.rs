//! Tock's central kernel logic and scheduler trait.
//!
//! Also defines several
//! utility functions to reduce repeated code between different scheduler
//! implementations.

pub(crate) mod cooperative;
pub(crate) mod mlfq;
pub(crate) mod priority;
pub(crate) mod round_robin;

use core::cell::Cell;
use core::ptr::NonNull;

use crate::callback::{AppId, Callback, CallbackId};
use crate::capabilities;
use crate::common::cells::NumericCellExt;
use crate::common::dynamic_deferred_call::DynamicDeferredCall;
use crate::config;
use crate::debug;
use crate::grant::Grant;
use crate::ipc;
use crate::memop;
use crate::platform::mpu::MPU;
use crate::platform::scheduler_timer::SchedulerTimer;
use crate::platform::watchdog::WatchDog;
use crate::platform::{Chip, Platform};
use crate::process::{self, Task};
use crate::returncode::ReturnCode;
use crate::syscall::{ContextSwitchReason, Syscall};

/// Skip re-scheduling a process if its quanta is nearly exhausted
pub(crate) const MIN_QUANTA_THRESHOLD_US: u32 = 500;

/// Trait which any scheduler must implement.
pub trait Scheduler<C: Chip> {
    /// Return the next process to run, and the timeslice length for that process.
    ///
    /// The first argument is an optional `AppId` of the process to run next.
    /// Returning `None` instead of `Some(AppId)` means nothing will run. The second
    /// argument is an optional number of microseconds to use as the length of the process's
    /// timeslice. If an `AppId` is returned in the first argument, then returning `None`
    /// instead of a timeslice will cause the process
    /// to be run cooperatively (i.e. without preemption). Otherwise the process will run
    /// with a timeslice set to the specified length.
    fn next(&self) -> (Option<AppId>, Option<u32>);

    /// Inform the scheduler of why the last process stopped executing, and how
    /// long it executed for. Notably, `execution_time_us` will be `None`
    /// if the the scheduler requested this process be run cooperatively.
    fn result(&self, result: StoppedExecutingReason, execution_time_us: Option<u32>);

    /// Tell the scheduler to execute kernel work such as interrupt bottom halves
    /// and dynamic deferred calls. Most schedulers will use this default
    /// implementation, but schedulers which at times wish to defer interrupt
    /// handling will reimplement it.
    ///
    /// Providing this interface allows schedulers to fully manage how
    /// the main kernel loop executes. For example, a more advanced
    /// scheduler that attempts to help processes meet their deadlines may
    /// need to defer bottom half interrupt handling or to selectively service
    /// certain interrupts. Or, a power aware scheduler may want to selectively
    /// choose what work to complete at any time to meet power requirements.
    ///
    /// Custom implementations of this function must be very careful, however,
    /// as this function is called in the core kernel loop.
    unsafe fn execute_kernel_work(&self, chip: &C) {
        chip.service_pending_interrupts();
        DynamicDeferredCall::call_global_instance_while(|| !chip.has_pending_interrupts());
    }

    /// Ask the scheduler whether to take a break from executing userspace
    /// processes to handle kernel tasks. Most schedulers will use this
    /// default implementation, which always prioritizes kernel work, but
    /// schedulers that wish to defer interrupt handling may reimplement it.
    unsafe fn break_for_kernel_tasks(&self, kernel: &Kernel, chip: &C) -> bool {
        chip.has_pending_interrupts()
            || DynamicDeferredCall::global_instance_calls_pending().unwrap_or(false)
            || kernel.processes_blocked()
    }

    /// Ask the scheduler whether to return from the loop in `do_process()`. Most
    /// schedulers will use this default implementation, which causes the do_process()
    /// loop to return if there are interrupts or deferred calls that need to be serviced.
    /// However, schedulers which wish to defer interrupt handling may change this, or
    /// priority schedulers which wish to check if the execution of the current process
    /// has caused a higher priority process to become ready (such as in the case of IPC)
    unsafe fn leave_do_process(&self, chip: &C) -> bool {
        chip.has_pending_interrupts()
            || DynamicDeferredCall::global_instance_calls_pending().unwrap_or(false)
    }
}

/// Main object for the kernel. Each board will need to create one.
pub struct Kernel {
    /// How many "to-do" items exist at any given time. These include
    /// outstanding callbacks and processes in the Running state.
    work: Cell<usize>,

    /// This holds a pointer to the static array of Process pointers.
    processes: &'static [Option<&'static dyn process::ProcessType>],

    /// A counter which keeps track of how many process identifiers have been
    /// created. This is used to create new unique identifiers for processes.
    process_identifier_max: Cell<usize>,

    /// How many grant regions have been setup. This is incremented on every
    /// call to `create_grant()`. We need to explicitly track this so that when
    /// processes are created they can allocated pointers for each grant.
    grant_counter: Cell<usize>,

    /// Flag to mark that grants have been finalized. This means that the kernel
    /// cannot support creating new grants because processes have already been
    /// created and the data structures for grants have already been
    /// established.
    grants_finalized: Cell<bool>,
}

/// Enum used to inform scheduler why a process stopped
/// executing (aka why `do_process()` returned).
#[derive(PartialEq, Eq)]
pub enum StoppedExecutingReason {
    /// The process returned because it is no longer ready to run
    NoWorkLeft,

    /// The process faulted, and the board restart policy was configured such that
    /// it was not restarted and there was not a kernel panic.
    StoppedFaulted,

    /// The kernel stopped the process
    Stopped,

    /// The process returned because its timeslice expired
    TimesliceExpired,

    /// The process returned because it was preempted by kernel
    /// work that became ready (most likely because an interrupt fired
    /// and the kernel thread needs to execute the bottom half of the
    /// interrupt)
    KernelPreemption,
}

impl Kernel {
    pub fn new(processes: &'static [Option<&'static dyn process::ProcessType>]) -> Kernel {
        Kernel {
            work: Cell::new(0),
            processes,
            process_identifier_max: Cell::new(0),
            grant_counter: Cell::new(0),
            grants_finalized: Cell::new(false),
        }
    }

    /// Something was scheduled for a process, so there is more work to do.
    ///
    /// This is only exposed in the core kernel crate.
    pub(crate) fn increment_work(&self) {
        self.work.increment();
    }

    /// Something was scheduled for a process, so there is more work to do.
    ///
    /// This is exposed publicly, but restricted with a capability. The intent
    /// is that external implementations of `ProcessType` need to be able to
    /// indicate there is more process work to do.
    pub fn increment_work_external(
        &self,
        _capability: &dyn capabilities::ExternalProcessCapability,
    ) {
        self.increment_work();
    }

    /// Something finished for a process, so we decrement how much work there is
    /// to do.
    ///
    /// This is only exposed in the core kernel crate.
    pub(crate) fn decrement_work(&self) {
        self.work.decrement();
    }

    /// Something finished for a process, so we decrement how much work there is
    /// to do.
    ///
    /// This is exposed publicly, but restricted with a capability. The intent
    /// is that external implementations of `ProcessType` need to be able to
    /// indicate that some process work has finished.
    pub fn decrement_work_external(
        &self,
        _capability: &dyn capabilities::ExternalProcessCapability,
    ) {
        self.decrement_work();
    }

    /// Helper function for determining if we should service processes or go to
    /// sleep.
    fn processes_blocked(&self) -> bool {
        self.work.get() == 0
    }

    /// Run a closure on a specific process if it exists. If the process with a
    /// matching `AppId` does not exist at the index specified within the
    /// `AppId`, then `default` will be returned.
    ///
    /// A match will not be found if the process was removed (and there is a
    /// `None` in the process array), if the process changed its identifier
    /// (likely after being restarted), or if the process was moved to a
    /// different index in the processes array. Note that a match _will_ be
    /// found if the process still exists in the correct location in the array
    /// but is in any "stopped" state.
    pub(crate) fn process_map_or<F, R>(&self, default: R, appid: AppId, closure: F) -> R
    where
        F: FnOnce(&dyn process::ProcessType) -> R,
    {
        // We use the index in the `appid` so we can do a direct lookup.
        // However, we are not guaranteed that the app still exists at that
        // index in the processes array. To avoid additional overhead, we do the
        // lookup and check here, rather than calling `.index()`.
        let tentative_index = appid.index;

        // Get the process at that index, and if it matches, run the closure
        // on it.
        self.processes
            .get(tentative_index)
            .map_or(None, |process_entry| {
                // Check if there is any process state here, or if the entry is
                // `None`.
                process_entry.map_or(None, |process| {
                    // Check that the process stored here matches the identifier
                    // in the `appid`.
                    if process.appid() == appid {
                        Some(closure(process))
                    } else {
                        None
                    }
                })
            })
            .unwrap_or(default)
    }

    /// Run a closure on every valid process. This will iterate the array of
    /// processes and call the closure on every process that exists.
    pub(crate) fn process_each<F>(&self, closure: F)
    where
        F: Fn(&dyn process::ProcessType),
    {
        for process in self.processes.iter() {
            match process {
                Some(p) => {
                    closure(*p);
                }
                None => {}
            }
        }
    }

    /// Returns an iterator over all processes loaded by the kernel
    pub(crate) fn get_process_iter(
        &self,
    ) -> core::iter::FilterMap<
        core::slice::Iter<Option<&dyn process::ProcessType>>,
        fn(&Option<&'static dyn process::ProcessType>) -> Option<&'static dyn process::ProcessType>,
    > {
        fn keep_some(
            &x: &Option<&'static dyn process::ProcessType>,
        ) -> Option<&'static dyn process::ProcessType> {
            x
        }
        self.processes.iter().filter_map(keep_some)
    }

    /// Run a closure on every valid process. This will iterate the array of
    /// processes and call the closure on every process that exists.
    ///
    /// This is functionally the same as `process_each()`, but this method is
    /// available outside the kernel crate and requires a
    /// `ProcessManagementCapability` to use.
    pub fn process_each_capability<F>(
        &'static self,
        _capability: &dyn capabilities::ProcessManagementCapability,
        closure: F,
    ) where
        F: Fn(&dyn process::ProcessType),
    {
        for process in self.processes.iter() {
            match process {
                Some(p) => {
                    closure(*p);
                }
                None => {}
            }
        }
    }

    /// Run a closure on every process, but only continue if the closure returns
    /// `FAIL`. That is, if the closure returns any other return code than
    /// `FAIL`, that value will be returned from this function and the iteration
    /// of the array of processes will stop.
    pub(crate) fn process_until<F>(&self, closure: F) -> ReturnCode
    where
        F: Fn(&dyn process::ProcessType) -> ReturnCode,
    {
        for process in self.processes.iter() {
            match process {
                Some(p) => {
                    let ret = closure(*p);
                    if ret != ReturnCode::FAIL {
                        return ret;
                    }
                }
                None => {}
            }
        }
        ReturnCode::FAIL
    }

    /// Retrieve the `AppId` of the given app based on its identifier. This is
    /// useful if an app identifier is passed to the kernel from somewhere (such
    /// as from userspace) and needs to be expanded to a full `AppId` for use
    /// with other APIs.
    pub(crate) fn lookup_app_by_identifier(&self, identifier: usize) -> Option<AppId> {
        self.processes.iter().find_map(|&p| {
            p.map_or(None, |p2| {
                if p2.appid().id() == identifier {
                    Some(p2.appid())
                } else {
                    None
                }
            })
        })
    }

    /// Checks if the provided `AppId` is still valid given the processes stored
    /// in the processes array. Returns `true` if the AppId still refers to
    /// a valid process, and `false` if not.
    ///
    /// This is needed for `AppId` itself to implement the `.index()` command to
    /// verify that the referenced app is still at the correct index.
    pub(crate) fn appid_is_valid(&self, appid: &AppId) -> bool {
        self.processes.get(appid.index).map_or(false, |p| {
            p.map_or(false, |process| process.appid().id() == appid.id())
        })
    }

    /// Create a new grant. This is used in board initialization to setup grants
    /// that capsules use to interact with processes.
    ///
    /// Grants **must** only be created _before_ processes are initialized.
    /// Processes use the number of grants that have been allocated to correctly
    /// initialize the process's memory with a pointer for each grant. If a
    /// grant is created after processes are initialized this will panic.
    ///
    /// Calling this function is restricted to only certain users, and to
    /// enforce this calling this function requires the
    /// `MemoryAllocationCapability` capability.
    pub fn create_grant<T: Default>(
        &'static self,
        _capability: &dyn capabilities::MemoryAllocationCapability,
    ) -> Grant<T> {
        if self.grants_finalized.get() {
            panic!("Grants finalized. Cannot create a new grant.");
        }

        // Create and return a new grant.
        let grant_index = self.grant_counter.get();
        self.grant_counter.increment();
        Grant::new(self, grant_index)
    }

    /// Returns the number of grants that have been setup in the system and
    /// marks the grants as "finalized". This means that no more grants can
    /// be created because data structures have been setup based on the number
    /// of grants when this function is called.
    ///
    /// In practice, this is called when processes are created, and the process
    /// memory is setup based on the number of current grants.
    pub(crate) fn get_grant_count_and_finalize(&self) -> usize {
        self.grants_finalized.set(true);
        self.grant_counter.get()
    }

    /// Returns the number of grants that have been setup in the system and
    /// marks the grants as "finalized". This means that no more grants can
    /// be created because data structures have been setup based on the number
    /// of grants when this function is called.
    ///
    /// In practice, this is called when processes are created, and the process
    /// memory is setup based on the number of current grants.
    ///
    /// This is exposed publicly, but restricted with a capability. The intent
    /// is that external implementations of `ProcessType` need to be able to
    /// retrieve the final number of grants.
    pub fn get_grant_count_and_finalize_external(
        &self,
        _capability: &dyn capabilities::ExternalProcessCapability,
    ) -> usize {
        self.get_grant_count_and_finalize()
    }

    /// Create a new unique identifier for a process and return the identifier.
    ///
    /// Typically we just choose a larger number than we have used for any process
    /// before which ensures that the identifier is unique.
    pub(crate) fn create_process_identifier(&self) -> usize {
        self.process_identifier_max.get_and_increment()
    }

    /// Cause all apps to fault.
    ///
    /// This will call `set_fault_state()` on each app, causing the app to enter
    /// the state as if it had crashed (for example with an MPU violation). If
    /// the process is configured to be restarted it will be.
    ///
    /// Only callers with the `ProcessManagementCapability` can call this
    /// function. This restricts general capsules from being able to call this
    /// function, since capsules should not be able to arbitrarily restart all
    /// apps.
    pub fn hardfault_all_apps<C: capabilities::ProcessManagementCapability>(&self, _c: &C) {
        for p in self.processes.iter() {
            p.map(|process| {
                process.set_fault_state();
            });
        }
    }

    pub fn kernel_loop<P: Platform, C: Chip, SC: Scheduler<C>>(
        &self,
        platform: &P,
        chip: &C,
        ipc: Option<&ipc::IPC>,
        scheduler: &SC,
        _capability: &dyn capabilities::MainLoopCapability,
    ) -> ! {
        loop {
            chip.watchdog().tickle();
            unsafe {
                scheduler.execute_kernel_work(chip);

                loop {
                    chip.watchdog().tickle();
                    if scheduler.break_for_kernel_tasks(self, chip) {
                        break;
                    }
                    let (appid, timeslice_us) = scheduler.next();
                    appid.map(|appid| {
                        self.process_map_or((), appid, |process| {
                            let (reason, time_executed) = self.do_process(
                                platform,
                                chip,
                                scheduler,
                                process,
                                ipc,
                                timeslice_us,
                            );
                            scheduler.result(reason, time_executed);
                        });
                    });
                }

                chip.atomic(|| {
                    if !chip.has_pending_interrupts()
                        && !DynamicDeferredCall::global_instance_calls_pending().unwrap_or(false)
                        && self.processes_blocked()
                    {
                        chip.watchdog().suspend();
                        chip.sleep();
                        chip.watchdog().resume();
                    }
                });
            };
        }
    }

    /// Transfer control from the scheduler to a userspace process.
    /// This function should be called by the scheduler to run userspace
    /// code. Notably, when processes make system calls, the system calls
    /// are handled in the kernel, *by the kernel thread*, but that is done
    /// by looping within this function. This function will only return
    /// control to the scheduler if a process yields with no callbacks pending,
    /// exceeds its timeslice, or is interrupted.
    ///
    /// Depending on the particular scheduler in use, this function can be configured
    /// to act in a few different ways. `break_for_kernel_tasks` allows the
    /// scheduler to tell the Kernel whether to return control to the scheduler as soon
    /// as a kernel task becomes ready (either a bottom half interrupt handler or
    /// dynamic deferred call), or to continue executing the userspace process
    /// until it reaches one of the aforementioned stopping conditions.
    /// Some schedulers may not require a systick, passing `None` for the timeslice
    /// will use a dummy systick rather than systick of the chip in use. Schedulers can
    /// pass a timeslice (in us) of their choice, though if the passed timeslice
    /// is smalled than MIN_QUANTA_THRESHOLD_US the process will not execute, and
    /// this function will return immediately.
    ///
    /// This function returns a tuple indicating the reason the reason this function
    /// has returned to the scheduler, and the amount of time the process spent
    /// executing (or None if the process was run cooperatively).
    /// Notably, time spent in this function by the kernel, executing system
    /// calls or merely setting up the switch to/from userspace, is charged to the
    /// process.
    unsafe fn do_process<P: Platform, C: Chip, S: Scheduler<C>>(
        &self,
        platform: &P,
        chip: &C,
        scheduler: &S,
        process: &dyn process::ProcessType,
        ipc: Option<&crate::ipc::IPC>,
        timeslice_us: Option<u32>,
    ) -> (StoppedExecutingReason, Option<u32>) {
        let scheduler_timer: &dyn SchedulerTimer = if timeslice_us.is_none() {
            &() //dummy timer, no preemption
        } else {
            chip.scheduler_timer()
        };
        scheduler_timer.reset();
        timeslice_us.map(|timeslice| scheduler_timer.start(timeslice));
        let mut return_reason = StoppedExecutingReason::NoWorkLeft;

        loop {
            if scheduler_timer.has_expired()
                || scheduler_timer.get_remaining_us() <= MIN_QUANTA_THRESHOLD_US
            {
                process.debug_timeslice_expired();
                return_reason = StoppedExecutingReason::TimesliceExpired;
                break;
            }

            if scheduler.leave_do_process(chip) {
                return_reason = StoppedExecutingReason::KernelPreemption;
                break;
            }

            match process.get_state() {
                process::State::Running => {
                    // Running means that this process expects to be running,
                    // so go ahead and set things up and switch to executing
                    // the process.
                    process.setup_mpu();
                    chip.mpu().enable_mpu();
                    scheduler_timer.arm();
                    let context_switch_reason = process.switch_to();
                    scheduler_timer.disarm();
                    chip.mpu().disable_mpu();

                    // Now the process has returned back to the kernel. Check
                    // why and handle the process as appropriate.
                    match context_switch_reason {
                        Some(ContextSwitchReason::Fault) => {
                            // Let process deal with it as appropriate.
                            process.set_fault_state();
                        }
                        Some(ContextSwitchReason::SyscallFired { syscall }) => {
                            process.debug_syscall_called(syscall);

                            // Enforce platform-specific syscall filtering here.
                            //
                            // Before continuing to handle non-yield syscalls
                            // the kernel first checks if the platform wants to
                            // block that syscall for the process, and if it
                            // does, sets a return value which is returned to
                            // the calling process.
                            //
                            // Filtering a syscall (i.e. blocking the syscall
                            // from running) does not cause the process to loose
                            // its timeslice. The error will be returned
                            // immediately (assuming the process has not already
                            // exhausted its timeslice) allowing the process to
                            // decide how to handle the error.
                            if syscall != Syscall::YIELD {
                                if let Err(response) = platform.filter_syscall(process, &syscall) {
                                    process.set_syscall_return_value(response.into());
                                    continue;
                                }
                            }

                            // Handle each of the syscalls.
                            match syscall {
                                Syscall::MEMOP { operand, arg0 } => {
                                    let res = memop::memop(process, operand, arg0);
                                    if config::CONFIG.trace_syscalls {
                                        debug!(
                                            "[{:?}] memop({}, {:#x}) = {:#x} = {:?}",
                                            process.appid(),
                                            operand,
                                            arg0,
                                            usize::from(res),
                                            res
                                        );
                                    }
                                    process.set_syscall_return_value(res.into());
                                }
                                Syscall::YIELD => {
                                    if config::CONFIG.trace_syscalls {
                                        debug!("[{:?}] yield", process.appid());
                                    }
                                    process.set_yielded_state();

                                    // There might be already enqueued callbacks
                                    continue;
                                }
                                Syscall::SUBSCRIBE {
                                    driver_number,
                                    subdriver_number,
                                    callback_ptr,
                                    appdata,
                                } => {
                                    let callback_id = CallbackId {
                                        driver_num: driver_number,
                                        subscribe_num: subdriver_number,
                                    };
                                    process.remove_pending_callbacks(callback_id);

                                    let callback = NonNull::new(callback_ptr).map(|ptr| {
                                        Callback::new(
                                            process.appid(),
                                            callback_id,
                                            appdata,
                                            ptr.cast(),
                                        )
                                    });

                                    let res =
                                        platform.with_driver(
                                            driver_number,
                                            |driver| match driver {
                                                Some(d) => d.subscribe(
                                                    subdriver_number,
                                                    callback,
                                                    process.appid(),
                                                ),
                                                None => ReturnCode::ENODEVICE,
                                            },
                                        );
                                    if config::CONFIG.trace_syscalls {
                                        debug!(
                                            "[{:?}] subscribe({:#x}, {}, @{:#x}, {:#x}) = {:#x} = {:?}",
                                            process.appid(),
                                            driver_number,
                                            subdriver_number,
                                            callback_ptr as usize,
                                            appdata,
                                            usize::from(res),
                                            res
                                        );
                                    }
                                    process.set_syscall_return_value(res.into());
                                }
                                Syscall::COMMAND {
                                    driver_number,
                                    subdriver_number,
                                    arg0,
                                    arg1,
                                } => {
                                    let res =
                                        platform.with_driver(
                                            driver_number,
                                            |driver| match driver {
                                                Some(d) => d.command(
                                                    subdriver_number,
                                                    arg0,
                                                    arg1,
                                                    process.appid(),
                                                ),
                                                None => ReturnCode::ENODEVICE,
                                            },
                                        );
                                    if config::CONFIG.trace_syscalls {
                                        debug!(
                                            "[{:?}] cmd({:#x}, {}, {:#x}, {:#x}) = {:#x} = {:?}",
                                            process.appid(),
                                            driver_number,
                                            subdriver_number,
                                            arg0,
                                            arg1,
                                            usize::from(res),
                                            res
                                        );
                                    }
                                    process.set_syscall_return_value(res.into());
                                }
                                Syscall::ALLOW {
                                    driver_number,
                                    subdriver_number,
                                    allow_address,
                                    allow_size,
                                } => {
                                    let res = platform.with_driver(driver_number, |driver| {
                                        match driver {
                                            Some(d) => {
                                                match process.allow(allow_address, allow_size) {
                                                    Ok(oslice) => d.allow(
                                                        process.appid(),
                                                        subdriver_number,
                                                        oslice,
                                                    ),
                                                    Err(err) => err, /* memory not valid */
                                                }
                                            }
                                            None => ReturnCode::ENODEVICE,
                                        }
                                    });
                                    if config::CONFIG.trace_syscalls {
                                        debug!(
                                            "[{:?}] allow({:#x}, {}, @{:#x}, {:#x}) = {:#x} = {:?}",
                                            process.appid(),
                                            driver_number,
                                            subdriver_number,
                                            allow_address as usize,
                                            allow_size,
                                            usize::from(res),
                                            res
                                        );
                                    }
                                    process.set_syscall_return_value(res.into());
                                }
                            }
                        }
                        Some(ContextSwitchReason::Interrupted) => {
                            if scheduler_timer.has_expired() {
                                // this interrupt was a timeslice expiration,
                                process.debug_timeslice_expired();
                                return_reason = StoppedExecutingReason::TimesliceExpired;
                                break;
                            }
                            // beginning of loop determines wheter to
                            // break to handle other processes, or continue executing
                            continue;
                        }
                        None => {
                            // Something went wrong when switching to this
                            // process. Indicate this by putting it in a fault
                            // state.
                            process.set_fault_state();
                        }
                    }
                }
                process::State::Yielded | process::State::Unstarted => match process.dequeue_task()
                {
                    // If the process is yielded it might be waiting for a
                    // callback. If there is a task scheduled for this process
                    // go ahead and set the process to execute it.
                    None => break,
                    Some(cb) => match cb {
                        Task::FunctionCall(ccb) => {
                            if config::CONFIG.trace_syscalls {
                                debug!(
                                    "[{:?}] function_call @{:#x}({:#x}, {:#x}, {:#x}, {:#x})",
                                    process.appid(),
                                    ccb.pc,
                                    ccb.argument0,
                                    ccb.argument1,
                                    ccb.argument2,
                                    ccb.argument3,
                                );
                            }
                            process.set_process_function(ccb);
                        }
                        Task::IPC((otherapp, ipc_type)) => {
                            ipc.map_or_else(
                                || {
                                    assert!(
                                        false,
                                        "Kernel consistency error: IPC Task with no IPC"
                                    );
                                },
                                |ipc| {
                                    ipc.schedule_callback(process.appid(), otherapp, ipc_type);
                                },
                            );
                        }
                    },
                },
                process::State::Fault => {
                    // We should never be scheduling a process in fault.
                    panic!("Attempted to schedule a faulty process");
                }
                process::State::StoppedRunning => {
                    return_reason = StoppedExecutingReason::Stopped;
                    break;
                    // Do nothing
                }
                process::State::StoppedYielded => {
                    return_reason = StoppedExecutingReason::Stopped;
                    break;
                    // Do nothing
                }
                process::State::StoppedFaulted => {
                    return_reason = StoppedExecutingReason::StoppedFaulted;
                    break;
                    // Do nothing
                }
            }
        }
        let time_executed_us = timeslice_us.map_or(None, |timeslice| {
            // min is to protect from wrapping
            Some(u32::min(
                timeslice - scheduler_timer.get_remaining_us(),
                timeslice,
            ))
        });
        scheduler_timer.reset();
        (return_reason, time_executed_us)
    }
}
