//! Windows process facts and control.
//!
//! * Identity: `GetProcessTimes` gives the creation time, so a reused pid is
//!   told apart from the service exactly as on Unix.
//! * Group: every service gets a named Job Object, `Local\taba-PID-START`.
//!   taba starts the process suspended, assigns it to the job, gives the
//!   process an inheritable handle to the job (so the job's name stays
//!   resolvable after taba exits, as long as a member holds the handle), and
//!   then resumes it. Children join the job automatically. The job plays the
//!   role of the Unix process group.
//! * Detaching: `CREATE_NO_WINDOW` gives the service its own hidden console,
//!   so closing the launching terminal does not take it down, and
//!   `CREATE_BREAKAWAY_FROM_JOB` takes it out of the launching terminal's job
//!   when that job allows it.
//! * Stopping: Ctrl+Break to the service's console process group (the
//!   graceful request; SIGBREAK for most runtimes), then `TerminateJobObject`
//!   after the timeout.
//! * Argv: `NtQueryInformationProcess(ProcessCommandLineInformation)` plus
//!   `CommandLineToArgvW`.
//! * Listening ports: `GetExtendedTcpTable` for IPv4 and IPv6.

use super::{Listener, ProcInfo, SpawnError, Spawned, StopResult, wait_for};
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use windows_sys::Wdk::System::Threading::{
    NtQueryInformationProcess, ProcessCommandLineInformation,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS,
    ERROR_INSUFFICIENT_BUFFER, FILETIME, GetLastError, HANDLE, HANDLE_FLAG_INHERIT,
    INVALID_HANDLE_VALUE, LocalFree, STILL_ACTIVE, SetHandleInformation, WAIT_OBJECT_0,
};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCPROW_OWNER_PID, TCP_TABLE_OWNER_PID_LISTENER,
};
use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};
use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
use windows_sys::Win32::System::Console::{
    AttachConsole, CTRL_BREAK_EVENT, FreeConsole, GenerateConsoleCtrlEvent, SetConsoleCtrlHandler,
};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOBOBJECT_BASIC_PROCESS_ID_LIST,
    JobObjectBasicProcessIdList, OpenJobObjectW, QueryInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CREATE_SUSPENDED,
    GetCurrentProcess, GetExitCodeProcess, GetProcessTimes, OpenProcess, OpenThread,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    QueryFullProcessImageNameW, ResumeThread, THREAD_SUSPEND_RESUME, TerminateProcess,
    WaitForSingleObject,
};
use windows_sys::Win32::UI::Shell::CommandLineToArgvW;

/// What `stop` uses after the timeout (for messages).
pub const KILL_SIGNAL: &str = "forced termination";

// Job object access rights (winnt.h).
const JOB_OBJECT_QUERY: u32 = 0x0004;
const JOB_OBJECT_TERMINATE: u32 = 0x0008;

/// An owned kernel handle, closed on drop.
struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

fn open_process(pid: i32, access: u32) -> Option<Handle> {
    if pid <= 0 {
        return None;
    }
    let h = unsafe { OpenProcess(access, 0, pid as u32) };
    (!h.is_null()).then_some(Handle(h))
}

/// FILETIME (100 ns ticks since 1601) to microseconds since the Unix epoch.
fn unix_micros(ft: FILETIME) -> u64 {
    let ticks = (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime);
    ticks.saturating_sub(116_444_736_000_000_000) / 10
}

fn creation_time(h: HANDLE) -> Option<u64> {
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut c, mut e, mut k, mut u) = (zero, zero, zero, zero);
    let ok = unsafe { GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) };
    (ok != 0).then(|| unix_micros(c))
}

/// Reads the creation time and liveness of `pid`. Returns `None` if no such
/// process can be opened by the current user.
pub fn info(pid: i32) -> Option<ProcInfo> {
    let (h, sync) = match open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE) {
        Some(h) => (h, true),
        None => (open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION)?, false),
    };
    let start_time = creation_time(h.0)?;
    // A process object outlives the process while handles to it are open;
    // such an exited process is reported like a Unix zombie.
    let exited = if sync {
        unsafe { WaitForSingleObject(h.0, 0) == WAIT_OBJECT_0 }
    } else {
        let mut code = 0u32;
        unsafe { GetExitCodeProcess(h.0, &mut code) != 0 && code != STILL_ACTIVE as u32 }
    };
    Some(ProcInfo {
        start_time,
        zombie: exited,
    })
}

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *const u16,
}

/// The command line of `pid`, split like the C runtime does.
pub fn argv(pid: i32) -> Option<Vec<String>> {
    let h = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    let mut len = 0u32;
    // u64 elements keep the UNICODE_STRING header aligned.
    let mut buf: Vec<u64> = vec![0; 64];
    loop {
        let status = unsafe {
            NtQueryInformationProcess(
                h.0,
                ProcessCommandLineInformation,
                buf.as_mut_ptr().cast(),
                (buf.len() * 8) as u32,
                &mut len,
            )
        };
        if status >= 0 {
            break;
        }
        let needed = (len as usize).div_ceil(8);
        if needed <= buf.len() || needed > 1 << 20 {
            return None;
        }
        buf.resize(needed, 0);
    }
    let us = unsafe { &*(buf.as_ptr() as *const UnicodeString) };
    if us.buffer.is_null() || us.length == 0 {
        return None;
    }
    let mut line: Vec<u16> =
        unsafe { std::slice::from_raw_parts(us.buffer, us.length as usize / 2) }.to_vec();
    line.push(0);
    let mut argc = 0i32;
    let list = unsafe { CommandLineToArgvW(line.as_ptr(), &mut argc) };
    if list.is_null() {
        return None;
    }
    let mut args = Vec::with_capacity(argc as usize);
    for i in 0..argc as usize {
        let p = unsafe { *list.add(i) };
        let n = (0..).take_while(|&j| unsafe { *p.add(j) } != 0).count();
        let s = unsafe { std::slice::from_raw_parts(p, n) };
        args.push(OsString::from_wide(s).to_string_lossy().into_owned());
    }
    unsafe { LocalFree(list.cast()) };
    if args.is_empty() { None } else { Some(args) }
}

/// File name of `pid`'s executable, e.g. `python.exe`.
fn image_name(pid: i32) -> Option<String> {
    let h = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    let mut buf = vec![0u16; 32_768];
    let mut size = buf.len() as u32;
    let ok =
        unsafe { QueryFullProcessImageNameW(h.0, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut size) };
    if ok == 0 {
        return None;
    }
    let path = PathBuf::from(OsString::from_wide(&buf[..size as usize]));
    path.file_name().map(|n| n.to_string_lossy().into_owned())
}

/// Pids listening on TCP `port` (IPv4 and IPv6), with their image names.
pub fn listeners(port: u16) -> Vec<Listener> {
    let mut pids: Vec<i32> = Vec::new();
    for af in [AF_INET, AF_INET6] {
        let Some(table) = tcp_listener_table(af) else {
            continue;
        };
        let count = u32::from_ne_bytes(table[..4].try_into().unwrap()) as usize;
        let (row_size, port_at, pid_at) = if af == AF_INET {
            (std::mem::size_of::<MIB_TCPROW_OWNER_PID>(), 8, 20)
        } else {
            (std::mem::size_of::<MIB_TCP6ROW_OWNER_PID>(), 20, 52)
        };
        for i in 0..count {
            let row = 4 + i * row_size;
            let Some(r) = table.get(row..row + row_size) else {
                break;
            };
            let field = |at: usize| u32::from_ne_bytes(r[at..at + 4].try_into().unwrap());
            // dwLocalPort holds the port in network byte order.
            let p = u16::from_be((field(port_at) & 0xffff) as u16);
            let pid = field(pid_at) as i32;
            if p == port && !pids.contains(&pid) {
                pids.push(pid);
            }
        }
    }
    pids.into_iter()
        .map(|pid| (pid, image_name(pid).unwrap_or_else(|| format!("pid {pid}"))))
        .collect()
}

fn tcp_listener_table(af: u16) -> Option<Vec<u8>> {
    let mut size = 0u32;
    for _ in 0..5 {
        let mut buf = vec![0u8; size as usize];
        let ptr = if buf.is_empty() {
            std::ptr::null_mut()
        } else {
            buf.as_mut_ptr().cast()
        };
        let rc = unsafe {
            GetExtendedTcpTable(
                ptr,
                &mut size,
                0,
                u32::from(af),
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            )
        };
        match rc {
            0 if !buf.is_empty() => return Some(buf),
            0 => return None,
            ERROR_INSUFFICIENT_BUFFER => size += 1024,
            _ => return None,
        }
    }
    None
}

// ---------------------------------------------------------------- jobs

fn job_name(pid: i32, start_time: u64) -> Vec<u16> {
    wide(OsStr::new(&format!("Local\\taba-{pid}-{start_time}")))
}

fn open_job(pid: i32, start_time: u64, access: u32) -> Option<Handle> {
    let name = job_name(pid, start_time);
    let h = unsafe { OpenJobObjectW(access, 0, name.as_ptr()) };
    (!h.is_null()).then_some(Handle(h))
}

/// Pids of the processes still running in `job`.
fn job_pids(job: &Handle) -> Vec<i32> {
    let mut cap = 256usize;
    loop {
        let header = std::mem::size_of::<JOBOBJECT_BASIC_PROCESS_ID_LIST>();
        let bytes = header + cap * std::mem::size_of::<usize>();
        let mut buf: Vec<usize> = vec![0; bytes.div_ceil(std::mem::size_of::<usize>())];
        let ok = unsafe {
            QueryInformationJobObject(
                job.0,
                JobObjectBasicProcessIdList,
                buf.as_mut_ptr().cast(),
                (buf.len() * std::mem::size_of::<usize>()) as u32,
                std::ptr::null_mut(),
            )
        };
        let list = unsafe { &*(buf.as_ptr() as *const JOBOBJECT_BASIC_PROCESS_ID_LIST) };
        let assigned = list.NumberOfAssignedProcesses as usize;
        if ok == 0 && assigned <= cap {
            return Vec::new();
        }
        if assigned > cap && cap < 1 << 16 {
            cap = assigned + 16;
            continue;
        }
        let n = list.NumberOfProcessIdsInList as usize;
        let ids = unsafe { std::slice::from_raw_parts(list.ProcessIdList.as_ptr(), n) };
        return ids.iter().map(|p| *p as i32).collect();
    }
}

/// Snapshot of all processes as (pid, parent pid).
fn process_tree() -> Vec<(i32, i32)> {
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snap == INVALID_HANDLE_VALUE {
        return Vec::new();
    }
    let snap = Handle(snap);
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut out = Vec::new();
    let mut more = unsafe { Process32FirstW(snap.0, &mut entry) } != 0;
    while more {
        out.push((entry.th32ProcessID as i32, entry.th32ParentProcessID as i32));
        more = unsafe { Process32NextW(snap.0, &mut entry) } != 0;
    }
    out
}

/// Live descendants of the leader `pid` started at `start_time`, found
/// through parent pids. Used when the job can no longer be opened by name
/// (every process holding its handle has exited). A child counts only if
/// it started after the leader and, when the leader's pid has been reused
/// since, before the reuse.
fn descendants(pid: i32, start_time: u64) -> Vec<i32> {
    let tree = process_tree();
    let mut found = Vec::new();
    let mut queue = vec![pid];
    while let Some(parent) = queue.pop() {
        let parent_start = info(parent).filter(|p| !p.zombie).map(|p| p.start_time);
        for &(child, ppid) in &tree {
            if ppid != parent || child == parent || child == pid || found.contains(&child) {
                continue;
            }
            let Some(c) = info(child).filter(|c| !c.zombie) else {
                continue;
            };
            // The console host of a console process is its child, but not
            // part of the service; it exits with the console's last client.
            if image_name(child).is_some_and(|n| n.eq_ignore_ascii_case("conhost.exe")) {
                continue;
            }
            let after_leader = c.start_time >= start_time;
            let before_reuse = match parent_start {
                Some(s) if parent == pid && s != start_time => c.start_time < s,
                Some(s) => c.start_time >= s,
                None => true,
            };
            if after_leader && before_reuse {
                found.push(child);
                queue.push(child);
            }
        }
    }
    found
}

/// The service's live processes, and its job if it can still be opened.
fn members(pid: i32, start_time: u64, access: u32) -> (Option<Handle>, Vec<i32>) {
    if let Some(job) = open_job(pid, start_time, access) {
        let pids = job_pids(&job);
        return (Some(job), pids);
    }
    let mut pids = descendants(pid, start_time);
    if super::is_alive(pid, start_time) {
        pids.insert(0, pid);
    }
    (None, pids)
}

/// Whether the service recorded with leader `pid` (== `pgid`) started at
/// `start_time` still runs: the leader itself, or any process of its job.
pub fn group_is_ours(pid: i32, _pgid: i32, start_time: u64) -> bool {
    if pid <= 0 {
        return false;
    }
    super::is_alive(pid, start_time) || !members(pid, start_time, JOB_OBJECT_QUERY).1.is_empty()
}

/// Whether process `pid` belongs to the service with leader `leader`.
pub fn in_group(pid: i32, leader: i32, _pgid: i32, start_time: u64) -> bool {
    if pid == leader {
        return super::is_alive(leader, start_time);
    }
    members(leader, start_time, JOB_OBJECT_QUERY)
        .1
        .contains(&pid)
}

/// Asks the service to exit with Ctrl+Break, waits up to `timeout`, then
/// terminates every process in its job.
pub fn stop_group(
    pid: i32,
    pgid: i32,
    start_time: u64,
    timeout: Duration,
) -> io::Result<StopResult> {
    let access = JOB_OBJECT_QUERY | JOB_OBJECT_TERMINATE;
    let (job, pids) = members(pid, start_time, access);
    if pids.is_empty() {
        return Ok(StopResult::Terminated);
    }
    let remaining = || match &job {
        Some(j) => job_pids(j),
        None => members(pid, start_time, access).1,
    };
    let step = Duration::from_millis(50);
    // Attach to the first member whose console can be joined (the job may
    // list the console host, which cannot be attached to).
    let sent = pids.iter().take(4).any(|p| send_ctrl_break(*p, pgid));
    if sent && wait_for(timeout, step, || remaining().is_empty()) {
        return Ok(StopResult::Terminated);
    }
    match &job {
        Some(j) => {
            if unsafe { TerminateJobObject(j.0, 1) } == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        None => {
            for p in remaining() {
                let h = open_process(p, PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION);
                if let Some(h) = h {
                    unsafe { TerminateProcess(h.0, 1) };
                }
            }
        }
    }
    wait_for(Duration::from_secs(2), Duration::from_millis(20), || {
        remaining().is_empty()
    });
    Ok(StopResult::Killed)
}

/// Sends Ctrl+Break to console process group `pgid`, through a helper
/// process (`taba __ctrl-break`) that attaches to the console of `attach`:
/// a process can only signal its own console, and taba must keep its own.
fn send_ctrl_break(attach: i32, pgid: i32) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let child = Command::new(exe)
        .args(["__ctrl-break", &attach.to_string(), &pgid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP)
        .spawn();
    let Ok(mut child) = child else {
        return false;
    };
    let mut status = None;
    wait_for(Duration::from_secs(5), Duration::from_millis(10), || {
        status = child.try_wait().ok().flatten();
        status.is_some()
    });
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    status.is_some_and(|s| s.success())
}

/// Body of the hidden `taba __ctrl-break ATTACH PGID` command. Returns the
/// exit code.
pub fn ctrl_break_helper(attach: u32, pgid: u32) -> u8 {
    unsafe {
        FreeConsole();
        if AttachConsole(attach) == 0 {
            return 2;
        }
        // Never act on the event ourselves.
        SetConsoleCtrlHandler(Some(ignore_ctrl), 1);
        // The console was created for the service (CREATE_NO_WINDOW), so its
        // process group is tried first, then every process on the console
        // (group 0), which also reaches members after the leader exited.
        let ok = GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pgid) != 0
            || GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, 0) != 0;
        FreeConsole();
        if ok { 0 } else { 3 }
    }
}

unsafe extern "system" fn ignore_ctrl(_ctrl: u32) -> windows_sys::core::BOOL {
    1
}

// ---------------------------------------------------------------- spawn

/// Resolves a bare program name the way a Windows shell does: the first
/// `PATH` entry holding NAME plus a `PATHEXT` extension (`npm` ->
/// `npm.cmd`). Names with a directory part are left alone.
pub fn resolve_program(name: &str) -> PathBuf {
    let p = Path::new(name);
    if name.contains(['/', '\\', ':']) {
        return p.to_path_buf();
    }
    let exts: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
        .split(';')
        .filter(|e| !e.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    let has_ext = p
        .extension()
        .is_some_and(|e| exts.contains(&format!(".{}", e.to_string_lossy().to_ascii_lowercase())));
    let Some(path) = std::env::var_os("PATH") else {
        return p.to_path_buf();
    };
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        if has_ext && dir.join(name).is_file() {
            return dir.join(name);
        }
        for ext in &exts {
            let c = dir.join(format!("{name}{ext}"));
            if c.is_file() {
                return c;
            }
        }
    }
    p.to_path_buf()
}

/// Spawns `cmd` detached (own hidden console, own console process group,
/// out of the caller's job if allowed) inside a new named job.
pub fn spawn_detached(mut cmd: Command) -> Result<Spawned, SpawnError> {
    // Child processes inherit every inheritable handle, including taba's own
    // stdin/stdout/stderr. A service holding the caller's output pipe would
    // keep `$(taba run ...)` or a harness reading taba's output waiting
    // forever, so taba's standard handles are made non-inheritable first.
    // The service gets its log and null stdin through `cmd`.
    for std_handle in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        let h = unsafe { GetStdHandle(std_handle) };
        if !h.is_null() && h != INVALID_HANDLE_VALUE {
            unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0) };
        }
    }
    let flags = CREATE_SUSPENDED | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW;
    cmd.creation_flags(flags | CREATE_BREAKAWAY_FROM_JOB);
    let child = match cmd.spawn() {
        // The caller's job does not allow breakaway: stay in it (nested).
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
            cmd.creation_flags(flags);
            cmd.spawn()
        }
        other => other,
    }
    .map_err(SpawnError::Start)?;
    let pid = child.id() as i32;
    let process = child.as_raw_handle() as HANDLE;
    match adopt(pid, process) {
        Ok(start_time) => Ok(Spawned {
            pid,
            pgid: pid,
            start_time,
        }),
        Err(e) => {
            unsafe { TerminateProcess(process, 1) };
            Err(SpawnError::Start(e))
        }
    }
}

/// Puts the suspended process into its job, hands it the job handle, and
/// resumes it. Returns its start time.
fn adopt(pid: i32, process: HANDLE) -> io::Result<u64> {
    let start_time =
        creation_time(process).ok_or_else(|| io::Error::other("cannot read its start time"))?;
    let name = job_name(pid, start_time);
    let job = unsafe { CreateJobObjectW(std::ptr::null(), name.as_ptr()) };
    if job.is_null() {
        return Err(io::Error::last_os_error());
    }
    let job = Handle(job);
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return Err(io::Error::other("its job object already exists"));
    }
    if unsafe { AssignProcessToJobObject(job.0, process) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // An inheritable handle inside the service keeps the job (and its name)
    // alive after taba exits, and children started with inheritance get it
    // too. The handle is intentionally never closed by taba.
    let mut remote: HANDLE = std::ptr::null_mut();
    let ok = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            job.0,
            process,
            &mut remote,
            0,
            1,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    resume(pid)?;
    Ok(start_time)
}

/// Resumes the threads of a process created suspended.
fn resume(pid: i32) -> io::Result<()> {
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snap == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let snap = Handle(snap);
    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
    let mut resumed = 0;
    let mut more = unsafe { Thread32First(snap.0, &mut entry) } != 0;
    while more {
        if entry.th32OwnerProcessID == pid as u32 {
            let t = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if !t.is_null() {
                let t = Handle(t);
                if unsafe { ResumeThread(t.0) } != u32::MAX {
                    resumed += 1;
                }
            }
        }
        more = unsafe { Thread32Next(snap.0, &mut entry) } != 0;
    }
    if resumed == 0 {
        return Err(io::Error::other("cannot resume its main thread"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_own_process() {
        let me = std::process::id() as i32;
        let info = info(me).expect("own process");
        assert!(!info.zombie);
        assert!(info.start_time > 1_500_000_000_000_000);
        assert!(argv(me).is_some_and(|a| !a.is_empty()));
        assert!(image_name(me).is_some_and(|n| n.ends_with(".exe")));
    }

    #[test]
    fn finds_own_listeners() {
        let me = std::process::id() as i32;
        let v4 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let p4 = v4.local_addr().unwrap().port();
        assert!(listeners(p4).iter().any(|(p, _)| *p == me));
        if let Ok(v6) = std::net::TcpListener::bind("[::1]:0") {
            let p6 = v6.local_addr().unwrap().port();
            assert!(listeners(p6).iter().any(|(p, _)| *p == me));
        }
    }

    #[test]
    fn resolves_pathext() {
        // cmd.exe is always on PATH.
        let p = resolve_program("cmd");
        assert!(
            p.to_string_lossy()
                .to_ascii_lowercase()
                .ends_with("cmd.exe"),
            "{p:?}"
        );
        assert_eq!(resolve_program(r"C:\x\y"), PathBuf::from(r"C:\x\y"));
    }
}
