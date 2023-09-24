use core::sync::atomic::compiler_fence;

use alloc::{boxed::Box, sync::Arc, vec::Vec};

use crate::{
    arch::CurrentIrqArch,
    exception::InterruptArch,
    include::bindings::bindings::MAX_CPU_NUM,
    kBUG,
    libs::{
        rbtree::RBTree,
        spinlock::{SpinLock, SpinLockGuard},
    },
    process::{ProcessControlBlock, ProcessFlags, ProcessManager, ProcessState, SchedEntity,TaskGroup},
    smp::core::smp_get_processor_id,
};

use super::{
    core::{sched_enqueue, Scheduler},
    SchedPriority,
};

/// 声明全局的cfs调度器实例
pub static mut CFS_SCHEDULER_PTR: Option<Box<SchedulerCFS>> = None;

///!1111
///pub static mut CFS_SCHEDULER_MANAGER: Vec<Option<Box<SchedulerCFS>>> = None;

/// @brief 获取cfs调度器实例的可变引用
#[inline]
pub fn __get_cfs_scheduler() -> &'static mut SchedulerCFS {
    return unsafe { CFS_SCHEDULER_PTR.as_mut().unwrap() };
}

/// @brief 初始化cfs调度器
pub unsafe fn sched_cfs_init() {
    if CFS_SCHEDULER_PTR.is_none() {
        CFS_SCHEDULER_PTR = Some(Box::new(SchedulerCFS::new()));
    } else {
        kBUG!("Try to init CFS Scheduler twice.");
        panic!("Try to init CFS Scheduler twice.");
    }
}

/// @brief CFS队列（per-cpu的）
#[derive(Debug)]
pub struct CFSQueue {
    /// 当前cpu上执行的进程剩余的时间片
    cpu_exec_proc_jiffies: i64,
    /// 自旋锁保护的队列
    locked_queue: SpinLock<RBTree<i64, Arc<SchedEntity>>>,
    /// 当前核心的队列专属的IDLE进程的pcb
    idle_pcb: Arc<ProcessControlBlock>,
}

impl CFSQueue {
    pub fn new(idle_pcb: Arc<ProcessControlBlock>) -> CFSQueue {
        CFSQueue {
            cpu_exec_proc_jiffies: 0,
            locked_queue: SpinLock::new(RBTree::new()),
            idle_pcb: idle_pcb,
        }
    }

    /// @brief 将pcb加入队列
    pub fn enqueue(&mut self, pcb: Arc<ProcessControlBlock>) {
        let mut queue = self.locked_queue.lock_irqsave();

        // 如果进程是IDLE进程，那么就不加入队列
        if pcb.pid().into() == 0 {
            return;
        }

        queue.insert(pcb.sched_info().virtual_runtime() as i64, pcb.clone());
    }

    /// @brief 将se加入队列
    pub fn enqueue_se(&mut self, se: Arc<SchedEntity>) {
        let mut queue = self.locked_queue.lock_irqsave();
        queue.insert(se.virtual_runtime() as i64, se.clone());
    }

    /// @brief 将pcb从调度队列中弹出,若队列为空，则返回IDLE进程的pcb
    pub fn dequeue(&mut self) -> Arc<ProcessControlBlock> {
        let res: Arc<ProcessControlBlock>;
        let mut queue = self.locked_queue.lock_irqsave();
        if !queue.is_empty() {
            // 队列不为空，返回下一个要执行的pcb
            res = queue.pop_first().unwrap().1;
        } else {
            // 如果队列为空，则返回IDLE进程的pcb
            res = self.idle_pcb.clone();
        }
        return res;
    }
    /// @brief 将pcb从调度队列中弹出
    /// !没有考虑idle_pcb
    pub fn dequeue_se(&mut self) -> Arc<SchedEntity> {
        let res: Arc<ProcessControlBlock>;
        let mut queue = self.locked_queue.lock_irqsave();
        if !queue.is_empty() {
            // 队列不为空，返回下一个要执行的task se
            res = queue.pop_first().unwrap().1;
        } 
        return res;
    }

    /// @brief 获取cfs队列的最小运行时间
    ///
    /// @return Option<i64> 如果队列不为空，那么返回队列中，最小的虚拟运行时间；否则返回None
    pub fn min_vruntime(
        queue: &SpinLockGuard<RBTree<i64, Arc<ProcessControlBlock>>>,
    ) -> Option<i64> {
        if !queue.is_empty() {
            return Some(queue.get_first().unwrap().1.sched_info().virtual_runtime() as i64);
        } else {
            return None;
        }
    }
    /// 获取运行队列的长度
    pub fn get_cfs_queue_size(
        queue: &SpinLockGuard<RBTree<i64, Arc<ProcessControlBlock>>>,
    ) -> usize {
        return queue.len();
    }
}

/// @brief CFS调度器类
pub struct SchedulerCFS {
    cpu_queue: Vec<&'static mut CFSQueue>,
}

impl SchedulerCFS {
    pub fn new() -> SchedulerCFS {
        // 暂时手动指定核心数目
        // todo: 从cpu模块来获取核心的数目
        let mut result = SchedulerCFS {
            cpu_queue: Default::default(),
        };

        // 为每个cpu核心创建队列，进程重构后可以直接初始化Idle_pcb？
        for i in 0..MAX_CPU_NUM {
            let idle_pcb = ProcessManager::idle_pcb()[i as usize].clone();
            result
                .cpu_queue
                .push(Box::leak(Box::new(CFSQueue::new(idle_pcb))));
        }

        return result;
    }

    pub fn get_cpu_queue(&self) -> Vec<&'static mut CFSQueue> {
        return self.cpu_queue;
    }

    /// @brief 更新这个cpu上，这个进程的可执行时间。
    #[inline]
    fn update_cpu_exec_proc_jiffies(
        _priority: SchedPriority,
        cfs_queue: &mut CFSQueue,
    ) -> &mut CFSQueue {
        // todo: 引入调度周期以及所有进程的优先权进行计算，然后设置分配给进程的可执行时间
        cfs_queue.cpu_exec_proc_jiffies = 10;

        return cfs_queue;
    }

    /// @brief 时钟中断到来时，由sched的core模块中的函数，调用本函数，更新CFS进程的可执行时间
    pub fn timer_update_jiffies(&mut self) {
        let current_cpu_queue: &mut CFSQueue = self.cpu_queue[smp_get_processor_id() as usize];
        // todo: 引入调度周期以及所有进程的优先权进行计算，然后设置进程的可执行时间

        // 更新进程的剩余可执行时间
        let queue = current_cpu_queue.locked_queue.lock();
        current_cpu_queue.cpu_exec_proc_jiffies -= 1;
        // 时间片耗尽，标记需要被调度
        if current_cpu_queue.cpu_exec_proc_jiffies <= 0 {
            ProcessManager::current_pcb()
                .flags()
                .insert(ProcessFlags::NEED_SCHEDULE);
        }
        drop(queue);

        // 更新当前进程的虚拟运行时间
        ProcessManager::current_pcb()
            .sched_info()
            .increase_virtual_runtime(1);
    }

    /// @brief 将进程加入cpu的cfs调度队列，并且重设其虚拟运行时间为当前队列的最小值
    pub fn enqueue_reset_vruntime(&mut self, pcb: Arc<ProcessControlBlock>) {
        let cpu_queue = &mut self.cpu_queue[pcb.sched_info().on_cpu().unwrap() as usize];
        let queue = cpu_queue.locked_queue.lock();
        if queue.len() > 0 {
            pcb.sched_info()
                .set_virtual_runtime(CFSQueue::min_vruntime(&queue).unwrap_or(0) as isize)
        }
        drop(queue);
        cpu_queue.enqueue(pcb);
    }

    ///@brief 将某进程的se添加到cfsqueue
    /// ! 应该将pcb的se添加到自己进程组下的Scheduler 的cfsqueue[cpu]中
    pub fn enqueue_se(&mut self , pcb: Arc<ProcessControlBlock>) {
        let cpu_queue = &mut self.cpu_queue[pcb.sched_info().on_cpu().unwrap() as usize];
        cpu_queue.enqueue_se(pcb.se());
    }


    ///@brief 在进程组创建时就将某进程组的se添加到cfsqueue
    pub fn enqueue_group_se(&mut self , tg_se: Arc<SchedEntity>,cpu:usize) {
        let cpu_queue = &mut self.cpu_queue[cpu];
        cpu_queue.enqueue_se(tg_se);
    }


    /// @brief 设置cpu的队列的IDLE进程的pcb
    #[allow(dead_code)]
    pub fn set_cpu_idle(&mut self, cpu_id: usize, pcb: Arc<ProcessControlBlock>) {
        // kdebug!("set cpu idle: id={}", cpu_id);
        self.cpu_queue[cpu_id].idle_pcb = pcb;
    }
    /// 获取某个cpu的运行队列中的进程数
    pub fn get_cfs_queue_len(&mut self, cpu_id: u32) -> usize {
        let queue = self.cpu_queue[cpu_id as usize].locked_queue.lock();
        return CFSQueue::get_cfs_queue_size(&queue);
    }

}

impl Scheduler for SchedulerCFS {
    /// @brief 在当前cpu上进行调度。
    /// 请注意，进入该函数之前，需要关中断
    ///! 返回 se
    fn sched(&mut self) -> Option<Arc<ProcessControlBlock>> {
        assert!(CurrentIrqArch::is_irq_enabled() == false);

        ProcessManager::current_pcb()
            .flags()
            .remove(ProcessFlags::NEED_SCHEDULE);

        let current_cpu_id = smp_get_processor_id() as usize;

        let current_cpu_queue: &mut CFSQueue = self.cpu_queue[current_cpu_id];

        
        //let proc: Arc<ProcessControlBlock> = current_cpu_queue.dequeue();
        let se:Arc<SchedEntity> = current_cpu_queue.dequeue_se();
        compiler_fence(core::sync::atomic::Ordering::SeqCst);
 
        ///! se中的调度信息的设置
        
        let mut cfs_queue: Option<CFSQueue> = None;
        while let Some(_) = current_cpu_queue.dequeue() {
            if cfs_queue.is_none() {
                break; // 当 cfs_queue 为空时即选到了要调度的进程，则退出循环
            }
            // 如果最优可运行实体是一个进程组
            cfs_queue = se.group_cfs_rq(); // 更新 cfs_queue 的值
        }
        

        let final_cpu_queue: &mut CFSQueue = cfs_queue[current_cpu_id];
        let proc: Arc<ProcessControlBlock> = se.pcb();
        // 如果当前不是running态，或者当前进程的虚拟运行时间大于等于下一个进程的，那就需要切换。
        if (ProcessManager::current_pcb().sched_info().state() != ProcessState::Runnable)
            || (ProcessManager::current_pcb().sched_info().virtual_runtime()
                >= proc.sched_info().virtual_runtime())
        {
            compiler_fence(core::sync::atomic::Ordering::SeqCst);
            // 本次切换由于时间片到期引发，则再次加入就绪队列，否则交由其它功能模块进行管理
            if ProcessManager::current_pcb().sched_info().state() == ProcessState::Runnable {
                sched_enqueue(ProcessManager::current_pcb(), false);
                compiler_fence(core::sync::atomic::Ordering::SeqCst);
            }
            compiler_fence(core::sync::atomic::Ordering::SeqCst);

            // 设置进程可以执行的时间
            //if current_cpu_queue.cpu_exec_proc_jiffies <= 0 {
            if final_cpu_queue.cpu_exec_proc_jiffies <= 0 {
                SchedulerCFS::update_cpu_exec_proc_jiffies(
                    proc.sched_info().priority(),
                    current_cpu_queue,
                );
            }

            compiler_fence(core::sync::atomic::Ordering::SeqCst);

            return Some(se);
        } else {
            // 不进行切换

            // 设置进程可以执行的时间
            compiler_fence(core::sync::atomic::Ordering::SeqCst);
            //if current_cpu_queue.cpu_exec_proc_jiffies <= 0 {
            if final_cpu_queue.cpu_exec_proc_jiffies <= 0 {
                SchedulerCFS::update_cpu_exec_proc_jiffies(
                    ProcessManager::current_pcb().sched_info().priority(),
                    //current_cpu_queue,
                    final_cpu_queue
                );
                // kdebug!("cpu:{:?}",current_cpu_id);
            }

            compiler_fence(core::sync::atomic::Ordering::SeqCst);
            sched_enqueue(proc, false);
            compiler_fence(core::sync::atomic::Ordering::SeqCst);
        }
        compiler_fence(core::sync::atomic::Ordering::SeqCst);

        return None;
    }

    fn enqueue_pcb(&mut self, pcb: Arc<ProcessControlBlock>) {
        let cpu_queue = &mut self.cpu_queue[pcb.sched_info().on_cpu().unwrap() as usize];

        cpu_queue.enqueue(pcb);
    }
}
