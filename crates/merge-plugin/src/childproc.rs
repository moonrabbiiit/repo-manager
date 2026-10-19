//! 子进程随主程序退出的兜底：svn / git 命令行客户端由本程序启动，主程序关闭时
//! 不能把它们留在后台继续跑（检测、上传等任务随之失去意义，还可能占着工作副本锁）。
//!
//! 实现：启动后建一个 Windows 任务对象（Job Object），打开「句柄关闭即终止全部
//! 进程」开关，再把每个 svn / git 子进程挂进去。任务对象句柄由主进程持有，随主
//! 进程退出由内核关闭——此刻仍在运行的子进程会被一并结束。正常关窗、崩溃、任务
//! 管理器结束进程三条路都覆盖，无需逐个记录 PID，也不依赖退出钩子是否来得及执行。
//!
//! 刻意不挂进任务对象的进程：
//! * Beyond Compare——对比窗口是给用户用的，程序关闭不应连带关掉；
//! * 自更新流程启动的新版本——旧进程退出正是新进程接管的时刻，杀掉等于自杀；
//! * reg / tasklist 等瞬时查询，进程本身毫秒级结束，无挂留问题。

/// 把已启动的子进程挂进主程序的任务对象。失败静默：兜底机制，不值得打扰用户。
pub fn attach(child: &std::process::Child) {
    imp::attach(child);
}

#[cfg(windows)]
mod imp {
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// 任务对象句柄：进程内建一次，终身不关（关闭即意味着要杀子进程）。
    /// 裸句柄指针本身不带 Send/Sync，包一层声明「跨线程共享这个句柄是安全的」——
    /// 句柄是内核对象引用，AssignProcessToJobObject 在任意线程调用都合法。
    #[derive(Clone, Copy)]
    struct SharedHandle(HANDLE);
    unsafe impl Send for SharedHandle {}
    unsafe impl Sync for SharedHandle {}

    static JOB: OnceLock<SharedHandle> = OnceLock::new();

    fn job() -> Option<HANDLE> {
        if let Some(handle) = JOB.get() {
            return Some(handle.0);
        }
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return None;
            }
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok == 0 {
                return None;
            }
            let _ = JOB.set(SharedHandle(handle));
            JOB.get().map(|h| h.0)
        }
    }

    pub fn attach(child: &std::process::Child) {
        use std::os::windows::io::AsRawHandle;
        unsafe {
            if let Some(job) = job() {
                AssignProcessToJobObject(job, child.as_raw_handle());
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn attach(_child: &std::process::Child) {}
}

#[cfg(test)]
mod tests {
    use super::attach;

    /// attach 必须接受任意已启动子进程且不 panic（真实挂载在无头环境也能工作，
    /// explorer 这类进程不挂任务对象，正好当测试素材，毫秒级退出无残留）。
    #[test]
    fn attach_survives_on_a_live_child() {
        let mut child = std::process::Command::new("cmd")
            .args(["/c", "exit"])
            .spawn()
            .expect("spawn test child");
        attach(&child);
        let _ = child.wait();
    }
}
