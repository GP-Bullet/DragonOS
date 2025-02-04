use core::{
    hash::{Hash, Hasher},
    intrinsics::{likely, unlikely},
    mem::ManuallyDrop,
    sync::atomic::{compiler_fence, AtomicBool, AtomicI32, AtomicIsize, AtomicUsize, Ordering},
};

use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use hashbrown::HashMap;

use crate::{
    arch::{process::ArchPCBInfo, sched::sched, CurrentIrqArch},
    exception::InterruptArch,
    filesystem::{
        procfs::procfs_unregister_pid,
        vfs::{file::FileDescriptorVec, FileType},
    },
    include::bindings::bindings::MAX_CPU_NUM,
    kdebug, kinfo,
    libs::{
        align::AlignedBox,
        casting::DowncastArc,
        rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
        wait_queue::WaitQueue,
    },
    mm::{percpu::PerCpuVar, set_INITIAL_PROCESS_ADDRESS_SPACE, ucontext::AddressSpace, VirtAddr},
    net::socket::SocketInode,
    sched::{
        cfs::{CFSQueue, SchedulerCFS},
        core::{enqueue_se, sched_enqueue, CPU_EXECUTING},
        SchedPolicy, SchedPriority,
    },
    smp::kick_cpu,
    syscall::SystemError,
};

use self::kthread::WorkerPrivate;

pub mod abi;
pub mod c_adapter;
pub mod exec;
pub mod fork;
pub mod idle;
pub mod init;
pub mod kthread;
pub mod process;
pub mod syscall;

/// 系统中所有进程的pcb
static ALL_PROCESS: SpinLock<Option<HashMap<Pid, Arc<ProcessControlBlock>>>> = SpinLock::new(None);
///系统中所有进程组的TaskGroup
static ALL_PROCESS_GROUP: SpinLock<Option<HashMap<Pid, Arc<TaskGroup>>>> = SpinLock::new(None);
pub static mut SWITCH_RESULT: Option<PerCpuVar<SwitchResult>> = None;

/// 一个只改变1次的全局变量，标志进程管理器是否已经初始化完成
static mut __PROCESS_MANAGEMENT_INIT_DONE: bool = false;

#[derive(Debug)]
pub struct SwitchResult {
    pub prev_pcb: Option<Arc<ProcessControlBlock>>,
    pub next_pcb: Option<Arc<ProcessControlBlock>>,
}

impl SwitchResult {
    pub fn new() -> Self {
        Self {
            prev_pcb: None,
            next_pcb: None,
        }
    }
}

#[derive(Debug)]
pub struct ProcessManager;
impl ProcessManager {
    fn init() {
        static INIT_FLAG: AtomicBool = AtomicBool::new(false);
        if INIT_FLAG
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            panic!("ProcessManager has been initialized!");
        }

        unsafe {
            compiler_fence(Ordering::SeqCst);
            kdebug!("To create address space for INIT process.");
            // test_buddy();
            set_INITIAL_PROCESS_ADDRESS_SPACE(
                AddressSpace::new(true).expect("Failed to create address space for INIT process."),
            );
            kdebug!("INIT process address space created.");
            compiler_fence(Ordering::SeqCst);
        };

        ALL_PROCESS.lock().replace(HashMap::new());
        Self::arch_init();
        kdebug!("process arch init done.");
        Self::init_idle();
        kdebug!("process idle init done.");

        unsafe {
            __PROCESS_MANAGEMENT_INIT_DONE = true;
        }
        kinfo!("Process Manager initialized.");
    }

    /// 获取当前进程的pcb
    pub fn current_pcb() -> Arc<ProcessControlBlock> {
        return ProcessControlBlock::arch_current_pcb();
    }

    /// 增加当前进程的锁持有计数
    #[inline(always)]
    pub fn preempt_disable() {
        if likely(unsafe { __PROCESS_MANAGEMENT_INIT_DONE }) {
            ProcessManager::current_pcb().preempt_disable();
        }
    }

    /// 减少当前进程的锁持有计数
    #[inline(always)]
    pub fn preempt_enable() {
        if likely(unsafe { __PROCESS_MANAGEMENT_INIT_DONE }) {
            ProcessManager::current_pcb().preempt_enable();
        }
    }

    /// 根据pid获取进程的pcb
    ///
    /// ## 参数
    ///
    /// - `pid` : 进程的pid
    ///
    /// ## 返回值
    ///
    /// 如果找到了对应的进程，那么返回该进程的pcb，否则返回None
    pub fn find(pid: Pid) -> Option<Arc<ProcessControlBlock>> {
        return ALL_PROCESS.lock().as_ref()?.get(&pid).cloned();
    }

    /// 向系统中添加一个进程的pcb
    ///
    /// ## 参数
    ///
    /// - `pcb` : 进程的pcb
    ///
    /// ## 返回值
    ///
    /// 无
    pub fn add_pcb(pcb: Arc<ProcessControlBlock>) {
        ALL_PROCESS
            .lock()
            .as_mut()
            .unwrap()
            .insert(pcb.pid(), pcb.clone());
    }

    /// 唤醒一个进程
    pub fn wakeup(pcb: &Arc<ProcessControlBlock>) -> Result<(), SystemError> {
        let _guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        let state = pcb.sched_info().state();
        if state.is_blocked() {
            let mut writer = pcb.sched_info_mut();
            let state = writer.state();
            if state.is_blocked() {
                writer.set_state(ProcessState::Runnable);
                // avoid deadlock
                drop(writer);

                sched_enqueue(pcb.clone(), true);
                return Ok(());
            } else if state.is_exited() {
                return Err(SystemError::EINVAL);
            } else {
                return Ok(());
            }
        } else if state.is_exited() {
            return Err(SystemError::EINVAL);
        } else {
            return Ok(());
        }
    }

    /// 标志当前进程永久睡眠，但是发起调度的工作，应该由调用者完成
    ///
    /// ## 注意
    ///
    /// - 进入当前函数之前，不能持有sched_info的锁
    /// - 进入当前函数之前，必须关闭中断
    pub fn mark_sleep(interruptable: bool) -> Result<(), SystemError> {
        assert_eq!(
            CurrentIrqArch::is_irq_enabled(),
            false,
            "interrupt must be disabled before enter ProcessManager::mark_sleep()"
        );

        let pcb = ProcessManager::current_pcb();
        let mut writer = pcb.sched_info_mut_irqsave();
        if writer.state() != ProcessState::Exited(0) {
            writer.set_state(ProcessState::Blocked(interruptable));
            pcb.flags().insert(ProcessFlags::NEED_SCHEDULE);
            drop(writer);

            return Ok(());
        }
        return Err(SystemError::EINTR);
    }

    /// 当子进程退出后向父进程发送通知
    fn exit_notify() {
        let current = ProcessManager::current_pcb();
        // 让INIT进程收养所有子进程
        if current.pid() != Pid(1) {
            unsafe {
                current
                    .adopt_childen()
                    .unwrap_or_else(|e| panic!("adopte_childen failed: error: {e:?}"))
            };
            // todo: 当信号机制重写后，这里需要向父进程发送SIGCHLD信号
        }
    }

    /// 退出当前进程
    ///
    /// ## 参数
    ///
    /// - `exit_code` : 进程的退出码
    pub fn exit(exit_code: usize) -> ! {
        // 关中断
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        let pcb = ProcessManager::current_pcb();
        pcb.sched_info
            .write()
            .set_state(ProcessState::Exited(exit_code));
        pcb.wait_queue.wakeup(Some(ProcessState::Blocked(true)));
        drop(pcb);
        ProcessManager::exit_notify();
        drop(irq_guard);
        sched();
        loop {}
    }

    pub unsafe fn release(pid: Pid) {
        let pcb = ProcessManager::find(pid);
        if !pcb.is_none() {
            let pcb = pcb.unwrap();
            // 判断该pcb是否在全局没有任何引用
            if Arc::strong_count(&pcb) <= 1 {
                drop(pcb);
                ALL_PROCESS.lock().as_mut().unwrap().remove(&pid);
            } else {
                // 如果不为1就panic
                panic!("pcb is still referenced");
            }
        }
    }

    /// 上下文切换完成后的钩子函数
    unsafe fn switch_finish_hook() {
        // kdebug!("switch_finish_hook");
        let prev_pcb = SWITCH_RESULT
            .as_mut()
            .unwrap()
            .get_mut()
            .prev_pcb
            .take()
            .expect("prev_pcb is None");
        let next_pcb = SWITCH_RESULT
            .as_mut()
            .unwrap()
            .get_mut()
            .next_pcb
            .take()
            .expect("next_pcb is None");

        // 由于进程切换前使用了SpinLockGuard::leak()，所以这里需要手动释放锁
        prev_pcb.arch_info.force_unlock();
        next_pcb.arch_info.force_unlock();
    }

    /// 如果目标进程正在目标CPU上运行，那么就让这个cpu陷入内核态
    ///
    /// ## 参数
    ///
    /// - `pcb` : 进程的pcb
    #[allow(dead_code)]
    pub fn kick(pcb: &Arc<ProcessControlBlock>) {
        ProcessManager::current_pcb().preempt_disable();
        let cpu_id = pcb.sched_info().on_cpu();

        if let Some(cpu_id) = cpu_id {
            let cpu_id = cpu_id;

            if pcb.pid() == CPU_EXECUTING.get(cpu_id) {
                kick_cpu(cpu_id).expect("ProcessManager::kick(): Failed to kick cpu");
            }
        }

        ProcessManager::current_pcb().preempt_enable();
    }
}

/// 上下文切换的钩子函数,当这个函数return的时候,将会发生上下文切换
pub unsafe extern "sysv64" fn switch_finish_hook() {
    ProcessManager::switch_finish_hook();
}

int_like!(Pid, AtomicPid, usize, AtomicUsize);

impl Hash for Pid {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl Pid {
    pub fn to_string(&self) -> String {
        self.0.to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessState {
    /// The process is running on a CPU or in a run queue.
    Runnable,
    /// The process is waiting for an event to occur.
    /// 其中的bool表示该等待过程是否可以被打断。
    /// - 如果该bool为true,那么，硬件中断/信号/其他系统事件都可以打断该等待过程，使得该进程重新进入Runnable状态。
    /// - 如果该bool为false,那么，这个进程必须被显式的唤醒，才能重新进入Runnable状态。
    Blocked(bool),
    /// 进程被信号终止
    // Stopped(SignalNumber),
    /// 进程已经退出，usize表示进程的退出码
    Exited(usize),
}

#[allow(dead_code)]
impl ProcessState {
    #[inline(always)]
    pub fn is_runnable(&self) -> bool {
        return matches!(self, ProcessState::Runnable);
    }

    #[inline(always)]
    pub fn is_blocked(&self) -> bool {
        return matches!(self, ProcessState::Blocked(_));
    }

    #[inline(always)]
    pub fn is_exited(&self) -> bool {
        return matches!(self, ProcessState::Exited(_));
    }
}

bitflags! {
    /// pcb的标志位
    pub struct ProcessFlags: usize {
        /// 当前pcb表示一个内核线程
        const KTHREAD = 1 << 0;
        /// 当前进程需要被调度
        const NEED_SCHEDULE = 1 << 1;
        /// 进程由于vfork而与父进程存在资源共享
        const VFORK = 1 << 2;
        /// 进程不可被冻结
        const NOFREEZE = 1 << 3;
        /// 进程正在退出
        const EXITING = 1 << 4;
        /// 进程由于接收到终止信号唤醒
        const WAKEKILL = 1 << 5;
        /// 进程由于接收到信号而退出.(Killed by a signal)
        const SIGNALED = 1 << 6;
        /// 进程需要迁移到其他cpu上
        const NEED_MIGRATE = 1 << 7;
    }
}

#[derive(Debug)]
pub struct ProcessControlBlock {
    /// 当前进程的pid
    pid: Pid,

    basic: RwLock<ProcessBasicInfo>,
    /// 当前进程的自旋锁持有计数
    preempt_count: AtomicUsize,

    flags: SpinLock<ProcessFlags>,
    worker_private: SpinLock<Option<WorkerPrivate>>,
    /// 进程的内核栈
    kernel_stack: RwLock<KernelStack>,

    /// 与调度相关的信息
    sched_info: RwLock<ProcessSchedulerInfo>,
    /// 与处理器架构相关的信息
    arch_info: SpinLock<ArchPCBInfo>,

    /// 父进程指针
    parent_pcb: RwLock<Weak<ProcessControlBlock>>,

    /// 子进程链表
    children: RwLock<HashMap<Pid, Arc<ProcessControlBlock>>>,

    /// 等待队列
    wait_queue: WaitQueue,
}

impl ProcessControlBlock {
    /// Generate a new pcb.
    ///
    /// ## 参数
    ///
    /// - `name` : 进程的名字
    /// - `kstack` : 进程的内核栈
    ///
    /// ## 返回值
    ///
    /// 返回一个新的pcb
    pub fn new(name: String, kstack: KernelStack) -> Arc<Self> {
        return Self::do_create_pcb(name, kstack, false);
    }

    /// 创建一个新的idle进程
    ///
    /// 请注意，这个函数只能在进程管理初始化的时候调用。
    pub fn new_idle(cpu_id: u32, kstack: KernelStack) -> Arc<Self> {
        let name = format!("idle-{}", cpu_id);
        return Self::do_create_pcb(name, kstack, true);
    }

    fn do_create_pcb(name: String, kstack: KernelStack, is_idle: bool) -> Arc<Self> {
        let (pid, ppid, cwd) = if is_idle {
            (Pid(0), Pid(0), "/".to_string())
        } else {
            (
                Self::generate_pid(),
                ProcessManager::current_pcb().pid(),
                ProcessManager::current_pcb().basic().cwd(),
            )
        };
        let pgid = ProcessManager::current_pcb().basic().pgid();
        let tg: Arc<TaskGroup> = ProcessManager::current_pcb().basic().tg();
        let basic_info = ProcessBasicInfo::new(pgid, ppid, name, cwd, None, Some(tg));
        let preempt_count = AtomicUsize::new(0);
        let flags = SpinLock::new(ProcessFlags::empty());

        let sched_info = ProcessSchedulerInfo::new(None);
        let arch_info = SpinLock::new(ArchPCBInfo::new(Some(&kstack)));

        let ppcb: Weak<ProcessControlBlock> = ProcessManager::find(ppid)
            .map(|p| Arc::downgrade(&p))
            .unwrap_or_else(|| Weak::new());

        let pcb = Self {
            pid,
            basic: basic_info,
            preempt_count,
            flags,
            kernel_stack: RwLock::new(kstack),
            worker_private: SpinLock::new(None),
            sched_info,
            arch_info,
            parent_pcb: RwLock::new(ppcb),
            children: RwLock::new(HashMap::new()),
            wait_queue: WaitQueue::INIT,
        };

        let pcb = Arc::new(pcb);

        // 设置进程的arc指针到内核栈的最低地址处
        unsafe { pcb.kernel_stack.write().set_pcb(Arc::clone(&pcb)).unwrap() };

        // 将当前pcb加入父进程的子进程哈希表中
        if pcb.pid() > Pid(1) {
            if let Some(ppcb_arc) = pcb.parent_pcb.read().upgrade() {
                let mut children = ppcb_arc.children.write();
                children.insert(pcb.pid(), pcb.clone());
            } else {
                panic!("parent pcb is None");
            }
        }

        return pcb;
    }

    /// 生成一个新的pid
    #[inline(always)]
    fn generate_pid() -> Pid {
        static NEXT_PID: AtomicPid = AtomicPid::new(Pid(1));
        return NEXT_PID.fetch_add(Pid(1), Ordering::SeqCst);
    }

    /// 返回当前进程的锁持有计数
    #[inline(always)]
    pub fn preempt_count(&self) -> usize {
        return self.preempt_count.load(Ordering::SeqCst);
    }

    /// 增加当前进程的锁持有计数
    #[inline(always)]
    pub fn preempt_disable(&self) {
        self.preempt_count.fetch_add(1, Ordering::SeqCst);
    }

    /// 减少当前进程的锁持有计数
    #[inline(always)]
    pub fn preempt_enable(&self) {
        self.preempt_count.fetch_sub(1, Ordering::SeqCst);
    }

    #[inline(always)]
    pub unsafe fn set_preempt_count(&self, count: usize) {
        self.preempt_count.store(count, Ordering::SeqCst);
    }

    #[inline(always)]
    pub fn flags(&self) -> SpinLockGuard<ProcessFlags> {
        return self.flags.lock();
    }

    #[inline(always)]
    pub fn basic(&self) -> RwLockReadGuard<ProcessBasicInfo> {
        return self.basic.read();
    }

    #[inline(always)]
    pub fn set_name(&self, name: String) {
        self.basic.write().set_name(name);
    }

    #[inline(always)]
    pub fn basic_mut(&self) -> RwLockWriteGuard<ProcessBasicInfo> {
        return self.basic.write();
    }

    #[inline(always)]
    pub fn arch_info(&self) -> SpinLockGuard<ArchPCBInfo> {
        return self.arch_info.lock();
    }

    #[inline(always)]
    pub fn arch_info_irqsave(&self) -> SpinLockGuard<ArchPCBInfo> {
        return self.arch_info.lock_irqsave();
    }

    #[inline(always)]
    pub fn kernel_stack(&self) -> RwLockReadGuard<KernelStack> {
        return self.kernel_stack.read();
    }

    #[inline(always)]
    #[allow(dead_code)]
    pub fn kernel_stack_mut(&self) -> RwLockWriteGuard<KernelStack> {
        return self.kernel_stack.write();
    }

    #[inline(always)]
    pub fn sched_info(&self) -> RwLockReadGuard<ProcessSchedulerInfo> {
        return self.sched_info.read();
    }

    #[inline(always)]
    pub fn sched_info_mut(&self) -> RwLockWriteGuard<ProcessSchedulerInfo> {
        return self.sched_info.write();
    }

    #[inline(always)]
    pub fn sched_info_mut_irqsave(&self) -> RwLockWriteGuard<ProcessSchedulerInfo> {
        return self.sched_info.write_irqsave();
    }

    #[inline(always)]
    pub fn worker_private(&self) -> SpinLockGuard<Option<WorkerPrivate>> {
        return self.worker_private.lock();
    }

    #[inline(always)]
    pub fn pid(&self) -> Pid {
        return self.pid;
    }

    /// 获取文件描述符表的Arc指针
    #[inline(always)]
    pub fn fd_table(&self) -> Arc<RwLock<FileDescriptorVec>> {
        return self.basic.read().fd_table().unwrap();
    }

    /// 根据文件描述符序号，获取socket对象的Arc指针
    ///
    /// ## 参数
    ///
    /// - `fd` 文件描述符序号
    ///
    /// ## 返回值
    ///
    /// Option(&mut Box<dyn Socket>) socket对象的可变引用. 如果文件描述符不是socket，那么返回None
    pub fn get_socket(&self, fd: i32) -> Option<Arc<SocketInode>> {
        let binding = ProcessManager::current_pcb().fd_table();
        let fd_table_guard = binding.read();

        let f = fd_table_guard.get_file_by_fd(fd)?;
        drop(fd_table_guard);

        let guard = f.lock();
        if guard.file_type() != FileType::Socket {
            return None;
        }
        let socket: Arc<SocketInode> = guard
            .inode()
            .downcast_arc::<SocketInode>()
            .expect("Not a socket inode");
        return Some(socket);
    }

    /// 当前进程退出时,让初始进程收养所有子进程
    unsafe fn adopt_childen(&self) -> Result<(), SystemError> {
        match ProcessManager::find(Pid(1)) {
            Some(init_pcb) => {
                let mut childen_guard = self.children.write();
                let mut init_childen_guard = init_pcb.children.write();

                childen_guard.drain().for_each(|(pid, child)| {
                    init_childen_guard.insert(pid, child);
                });

                return Ok(());
            }
            _ => Err(SystemError::ECHILD),
        }
    }

    /// 生成进程的名字
    pub fn generate_name(_program_path: &str, args: &Vec<String>) -> String {
        let mut name = "".to_string();
        for arg in args {
            name.push_str(arg);
            name.push(' ');
        }
        return name;
    }
}

impl Drop for ProcessControlBlock {
    fn drop(&mut self) {
        // 在ProcFS中,解除进程的注册
        procfs_unregister_pid(self.pid())
            .unwrap_or_else(|e| panic!("procfs_unregister_pid failed: error: {e:?}"));

        if let Some(ppcb) = self.parent_pcb.read().upgrade() {
            ppcb.children.write().remove(&self.pid());
        }

        unsafe { ProcessManager::release(self.pid()) };
    }
}
/// 进程的基本信息
///
/// 这个结构体保存进程的基本信息，主要是那些不会随着进程的运行而经常改变的信息。
#[derive(Debug)]
pub struct ProcessBasicInfo {
    /// 当前进程的进程组id
    pgid: Pid,
    /// 当前进程的父进程的pid
    ppid: Pid,
    /// 进程的名字
    name: String,

    /// 当前进程的工作目录
    cwd: String,

    /// 用户地址空间
    user_vm: Option<Arc<AddressSpace>>,

    /// 文件描述符表
    fd_table: Option<Arc<RwLock<FileDescriptorVec>>>,

    /// 所在的进程组tg
    sched_tg: Option<Arc<TaskGroup>>,
}

impl ProcessBasicInfo {
    pub fn new(
        pgid: Pid,
        ppid: Pid,
        name: String,
        cwd: String,
        user_vm: Option<Arc<AddressSpace>>,
        sched_tg: Option<Arc<TaskGroup>>,
    ) -> RwLock<Self> {
        let fd_table = Arc::new(RwLock::new(FileDescriptorVec::new()));
        return RwLock::new(Self {
            pgid,
            ppid,
            name,
            cwd,
            user_vm,
            fd_table: Some(fd_table),
            sched_tg: Some(sched_tg),
        });
    }

    pub fn pgid(&self) -> Pid {
        return self.pgid;
    }

    pub fn set_pgid(&self, npgid: Pid) -> Pid {
        self.pgid = npgid;
    }

    pub fn ppid(&self) -> Pid {
        return self.ppid;
    }

    pub fn name(&self) -> &str {
        return &self.name;
    }

    pub fn set_name(&mut self, name: String) {
        self.name = name;
    }

    pub fn cwd(&self) -> String {
        return self.cwd.clone();
    }
    pub fn set_cwd(&mut self, path: String) {
        return self.cwd = path;
    }

    pub fn user_vm(&self) -> Option<Arc<AddressSpace>> {
        return self.user_vm.clone();
    }

    pub unsafe fn set_user_vm(&mut self, user_vm: Option<Arc<AddressSpace>>) {
        self.user_vm = user_vm;
    }

    pub fn fd_table(&self) -> Option<Arc<RwLock<FileDescriptorVec>>> {
        return self.fd_table.clone();
    }

    pub fn set_fd_table(&mut self, fd_table: Option<Arc<RwLock<FileDescriptorVec>>>) {
        self.fd_table = fd_table;
    }

    pub fn set_tg(&mut self, tg: Option<Arc<TaskGroup>>) {
        self.sched_tg = tg;
    }

    pub fn tg(&self) -> Option<Arc<TaskGroup>> {
        return self.sched_tg.clone();
    }
}

#[derive(Debug)]
pub struct ProcessSchedulerInfo {
    /// 当前进程所在的cpu
    on_cpu: AtomicI32,
    /// 如果当前进程等待被迁移到另一个cpu核心上（也就是flags中的PF_NEED_MIGRATE被置位），
    /// 该字段存储要被迁移到的目标处理器核心号
    migrate_to: AtomicI32,

    /// 当前进程的状态
    state: ProcessState,
    /// 进程的调度策略
    sched_policy: SchedPolicy,
    /// 进程的调度优先级
    priority: SchedPriority,
    /// 当前进程的虚拟运行时间
    virtual_runtime: AtomicIsize,
    /// 由实时调度器管理的时间片
    rt_time_slice: AtomicIsize,
    //调度实体
    se: Option<Arc<SchedEntity>>,
}

impl ProcessSchedulerInfo {
    pub fn new(on_cpu: Option<u32>) -> RwLock<Self> {
        let cpu_id = match on_cpu {
            Some(cpu_id) => cpu_id as i32,
            None => -1,
        };
        return RwLock::new(Self {
            on_cpu: AtomicI32::new(cpu_id),
            migrate_to: AtomicI32::new(-1),
            state: ProcessState::Blocked(false),
            sched_policy: SchedPolicy::CFS,
            virtual_runtime: AtomicIsize::new(0),
            rt_time_slice: AtomicIsize::new(0),
            priority: SchedPriority::new(100).unwrap(),
            se: Some(Arc::new(SchedEntity::new())),
        });
    }

    pub fn on_cpu(&self) -> Option<u32> {
        let on_cpu = self.on_cpu.load(Ordering::SeqCst);
        if on_cpu == -1 {
            return None;
        } else {
            return Some(on_cpu as u32);
        }
    }

    pub fn set_on_cpu(&self, on_cpu: Option<u32>) {
        if let Some(cpu_id) = on_cpu {
            self.on_cpu.store(cpu_id as i32, Ordering::SeqCst);
        } else {
            self.on_cpu.store(-1, Ordering::SeqCst);
        }
    }

    pub fn migrate_to(&self) -> Option<u32> {
        let migrate_to = self.migrate_to.load(Ordering::SeqCst);
        if migrate_to == -1 {
            return None;
        } else {
            return Some(migrate_to as u32);
        }
    }

    pub fn set_migrate_to(&self, migrate_to: Option<u32>) {
        if let Some(data) = migrate_to {
            self.migrate_to.store(data as i32, Ordering::SeqCst);
        } else {
            self.migrate_to.store(-1, Ordering::SeqCst)
        }
    }

    pub fn state(&self) -> ProcessState {
        return self.state;
    }

    fn set_state(&mut self, state: ProcessState) {
        self.state = state;
    }

    pub fn policy(&self) -> SchedPolicy {
        return self.sched_policy;
    }

    pub fn virtual_runtime(&self) -> isize {
        return self.virtual_runtime.load(Ordering::SeqCst);
    }

    pub fn set_virtual_runtime(&self, virtual_runtime: isize) {
        self.virtual_runtime
            .store(virtual_runtime, Ordering::SeqCst);
    }
    pub fn increase_virtual_runtime(&self, delta: isize) {
        self.virtual_runtime.fetch_add(delta, Ordering::SeqCst);
    }

    pub fn rt_time_slice(&self) -> isize {
        return self.rt_time_slice.load(Ordering::SeqCst);
    }

    pub fn set_rt_time_slice(&self, rt_time_slice: isize) {
        self.rt_time_slice.store(rt_time_slice, Ordering::SeqCst);
    }

    pub fn increase_rt_time_slice(&self, delta: isize) {
        self.rt_time_slice.fetch_add(delta, Ordering::SeqCst);
    }

    pub fn priority(&self) -> SchedPriority {
        return self.priority;
    }

    pub fn se(&self) -> Option<Arc<SchedEntity>> {
        return self.se.clone();
    }
}

#[derive(Debug)]
pub struct KernelStack {
    stack: Option<AlignedBox<[u8; KernelStack::SIZE], { KernelStack::ALIGN }>>,
    /// 标记该内核栈是否可以被释放
    can_be_freed: bool,
}

impl KernelStack {
    pub const SIZE: usize = 0x4000;
    pub const ALIGN: usize = 0x4000;

    pub fn new() -> Result<Self, SystemError> {
        return Ok(Self {
            stack: Some(
                AlignedBox::<[u8; KernelStack::SIZE], { KernelStack::ALIGN }>::new_zeroed()?,
            ),
            can_be_freed: true,
        });
    }

    /// 根据已有的空间，构造一个内核栈结构体
    ///
    /// 仅仅用于BSP启动时，为idle进程构造内核栈。其他时候使用这个函数，很可能造成错误！
    pub unsafe fn from_existed(base: VirtAddr) -> Result<Self, SystemError> {
        if base.is_null() || base.check_aligned(Self::ALIGN) == false {
            return Err(SystemError::EFAULT);
        }

        return Ok(Self {
            stack: Some(
                AlignedBox::<[u8; KernelStack::SIZE], { KernelStack::ALIGN }>::new_unchecked(
                    base.data() as *mut [u8; KernelStack::SIZE],
                ),
            ),
            can_be_freed: false,
        });
    }

    /// 返回内核栈的起始虚拟地址(低地址)
    pub fn start_address(&self) -> VirtAddr {
        return VirtAddr::new(self.stack.as_ref().unwrap().as_ptr() as usize);
    }

    /// 返回内核栈的结束虚拟地址(高地址)(不包含该地址)
    pub fn stack_max_address(&self) -> VirtAddr {
        return VirtAddr::new(self.stack.as_ref().unwrap().as_ptr() as usize + Self::SIZE);
    }

    pub unsafe fn set_pcb(&mut self, pcb: Arc<ProcessControlBlock>) -> Result<(), SystemError> {
        // 将一个Arc<ProcessControlBlock>放到内核栈的最低地址处
        let p: *const ProcessControlBlock = Arc::into_raw(pcb);
        let stack_bottom_ptr = self.start_address().data() as *mut *const ProcessControlBlock;

        // 如果内核栈的最低地址处已经有了一个pcb，那么，这里就不再设置,直接返回错误
        if unlikely(unsafe { !(*stack_bottom_ptr).is_null() }) {
            return Err(SystemError::EPERM);
        }
        // 将pcb的地址放到内核栈的最低地址处
        unsafe {
            *stack_bottom_ptr = p;
        }

        return Ok(());
    }

    /// 返回指向当前内核栈pcb的Arc指针
    #[allow(dead_code)]
    pub unsafe fn pcb(&self) -> Option<Arc<ProcessControlBlock>> {
        // 从内核栈的最低地址处取出pcb的地址
        let p = self.stack.as_ref().unwrap().as_ptr() as *const ProcessControlBlock;
        if unlikely(p.is_null()) {
            return None;
        }

        // 为了防止内核栈的pcb指针被释放，这里需要将其包装一下，使得Arc的drop不会被调用
        let arc_wrapper: ManuallyDrop<Arc<ProcessControlBlock>> =
            ManuallyDrop::new(Arc::from_raw(p));

        let new_arc: Arc<ProcessControlBlock> = Arc::clone(&arc_wrapper);
        return Some(new_arc);
    }
}

impl Drop for KernelStack {
    fn drop(&mut self) {
        if !self.stack.is_none() {
            let pcb_ptr: Arc<ProcessControlBlock> = unsafe {
                Arc::from_raw(self.stack.as_ref().unwrap().as_ptr() as *const ProcessControlBlock)
            };
            drop(pcb_ptr);
        }
        // 如果该内核栈不可以被释放，那么，这里就forget，不调用AlignedBox的drop函数
        if !self.can_be_freed {
            let bx = self.stack.take();
            core::mem::forget(bx);
        }
    }
}

pub fn process_init() {
    ProcessManager::init();
}

#[derive(Debug)]
pub struct SchedEntity {
    ///如果是进程它将对应一个pcb
    pcb: Option<Arc<ProcessControlBlock>>,
    /// 进程的调度优先级
    priority: SchedPriority,
    //parent:SchedEntity,
    /// 当前进程的虚拟运行时间
    virtual_runtime: AtomicIsize,
    cfs_rq: SpinLock<Option<CFSQueue>>,
    ///是否判断是否有进程组
    my_q: SpinLock<Option<CFSQueue>>,
}
///! CFSQueue 需要处理
///! parent 当为task se 指向进程组
///!        当为group se 指向父进程组

impl SchedEntity {
    pub fn new() -> RwLock<Self> {
        return RwLock::new(Self {
            pcb: None,
            virtual_runtime: AtomicIsize::new(0),
            priority: SchedPriority::new(100).unwrap(),
            my_q: None,
            cfs_rq: None,
        });
    }
    pub fn virtual_runtime(&self) -> isize {
        return self.virtual_runtime.load(Ordering::SeqCst);
    }

    pub fn set_virtual_runtime(&self, virtual_runtime: isize) {
        self.virtual_runtime
            .store(virtual_runtime, Ordering::SeqCst);
    }

    fn set_cfs_rq(&self, cfs_rq: Option<CFSQueue>) {
        self.cfs_rq.lock().replace(cfs_rq);
    }

    fn cfs_rq(&self) -> SpinLockGuard<Option<CFSQueue>> {
        return self.cfs_rq.lock();
    }

    fn set_my_q(&self, my_q: Option<CFSQueue>) {
        self.my_q.lock().replace(my_q);
    }

    pub fn group_cfs_rq(&self) -> SpinLockGuard<Option<CFSQueue>> {
        return self.my_q.lock();
    }

    pub fn priority(&self) -> SchedPriority {
        return self.priority;
    }

    pub fn set_pcb(&mut self, pcb: Option<Arc<ProcessControlBlock>>) {
        self.pcb = pcb;
    }

    pub fn pcb(&self) -> Option<Arc<ProcessControlBlock>> {
        return self.pcb.as_ref();
    }
}
#[derive(Debug)]
struct TaskGroup {
    se: Vec<Arc<SchedEntity>>, // 调度实体(每个CPU分配一个)
    cfs: SchedulerCFS,         // 完全公平调度运行队列(每个CPU分配一个)
    /// 父进程指针
    parent_tg: RwLock<Weak<TaskGroup>>,
    /// 子进程组链表
    children: RwLock<HashMap<Pid, Arc<TaskGroup>>>,
}

impl TaskGroup {
    pub fn new(ptg: Arc<TaskGroup>) -> Arc<Self> {
        return Arc::new(Self {
            cfs: SchedulerCFS::new(),
            se: for i in 0..MAX_CPU_NUM {
                Arc::new(SchedEntity::new())
            },
            parent_tg: RwLock::new(ptg),
            children: RwLock::new(HashMap::new()),
        });
    }
    /// ! 在创建新的进程组时，应该将se的cfsrq指向父进程组的cfsrq，cfsrq[cpu]
    /// !   同时my_q应指向本进程组创建的cfsrq
    /// !根进程组的创建时机  struct task_group root_task_group;
    /// ! 创建好关系之后 ，group se 和时task se 加到对应的cfs_rq
    pub fn init_group_se(&self, ptg: Arc<TaskGroup>, ntg: Arc<TaskGroup>) {
        let cfs_queue = ptg.cfs.get_cpu_queue();
        let ncfs_queue = ntg.cfs.get_cpu_queue();
        for cpu in 0..MAX_CPU_NUM {
            if let Some(per_se) = self.se.get(cpu) {
                per_se.set_cfs_rq(cfs_queue[cpu]);
                per_se.set_my_q(ncfs_queue[cpu]);
                ntg.cfs.enqueue_group_se(per_se, cpu);
            }
        }
    }

    pub fn add_tg(pgid: Pid, tg: Arc<TaskGroup>) {
        ALL_PROCESS_GROUP
            .lock()
            .as_mut()
            .unwrap()
            .insert(pgid, tg.clone());
    }

    pub fn cfs(&self) -> SchedulerCFS {
        return self.cfs;
    }
}

pub static PROCESS_GROUP_MANAGER: ProcessGroupManager = ProcessGroupManager::new();

pub struct ProcessGroupManager(pub SpinLock<BTreeMap<Pid, Vec<Pid>>>);

impl ProcessGroupManager {
    pub const fn new() -> Self {
        Self(SpinLock::new(BTreeMap::new()))
    }

    /// 根据pgid获取进程组的tg
    ///
    /// ## 参数
    /// - `pgid` : 进程的pgid
    ///
    /// ## 返回值
    /// 如果找到了对应的进程组，那么返回该进程组，否则返回None
    pub fn find(pgid: Pid) -> Option<Arc<TaskGroup>> {
        return ALL_PROCESS_GROUP.lock().as_ref()?.get(&pgid).cloned();
    }

    pub fn add_process(&self, pgid: Pid, pid: Pid) {
        let mut inner = self.0.lock();
        let vec = inner.get(&pgid);
        let mut vec = vec.cloned().unwrap();
        vec.push(pid);
        inner.insert(pgid, vec);
    }

    pub fn add_group(&self, pgid: Pid) {
        let mut inner = self.0.lock();
        let mut vec: Vec<Pid> = Vec::new();
        if pgid != Pid(0) {
            //? 不清楚
            vec.push(pgid);
        }
        inner.insert(pgid, vec);
    }

    pub fn get_group_by_pgid(&self, pgid: Pid) -> Vec<Pid> {
        self.0.lock().get(&pgid).cloned().unwrap()
    }

    ///return ornewtg
    pub fn set_pgid_by_pid(&self, pid: Pid, new_pgid: Pid, old_pgid: Pid) -> bool {
        let mut inner = self.0.lock();
        let old_group_vec = inner.get_mut(&old_pgid).unwrap();
        old_group_vec.retain(|&x| x != pid);
        let new_group_vec = inner.get_mut(&new_pgid);
        if let Some(new_group_vec) = new_group_vec {
            new_group_vec.push(pid);
            return false;
        } else {
            let new_group: Vec<Pid> = vec![new_pgid];
            // let mut new_group = Vec::new();
            // new_group.push(new_pgid);
            inner.insert(new_pgid, new_group);
            return true;
        }
    }
}
