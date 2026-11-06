//! Truly independent checks (spec §5).
//!
//! The service copies candidate blobs into a scratch workdir, runs user-
//! approved check profiles there, and treats results as valid only when
//! every input file is byte-identical after the run. Process trees are
//! managed with Job Objects on Windows (suspended-create → assign → resume
//! → kill-on-close); the environment is a minimal allowlist; stdout/stderr
//! are bounded, secret-redacted, and never trusted for verdicts ("PASS"
//! text is data, exit code + unchanged inputs are the verdict).

use crate::errors::{Code, RelayError, RelayResult};
use crate::model::{CheckProfile, CheckResult, VerificationRecord};
use crate::pathsafe;
use crate::secrets;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub const DEFAULT_STEP_TIMEOUT_SECONDS: u64 = 600;
pub const ROUND_TIMEOUT_SECONDS: u64 = 20 * 60;
pub const OUTPUT_CAP_BYTES: usize = 256 * 1024;
pub const STOP_GRACE_MS: u64 = 5000;

#[derive(Debug, Clone)]
pub struct CancelFlag(Arc<AtomicBool>);

impl CancelFlag {
    pub fn new() -> CancelFlag {
        CancelFlag(Arc::new(AtomicBool::new(false)))
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

// ---------------------------------------------------------------------------
// Environment allowlist
// ---------------------------------------------------------------------------

const ENV_ALLOWLIST: &[&str] = &[
    "SYSTEMROOT",
    "SYSTEMDRIVE",
    "TEMP",
    "TMP",
    "PATH",
    "PATHEXT",
    "COMSPEC",
    "HOMEDRIVE",
    "HOMEPATH",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMFILES",
    "COMMONPROGRAMFILES",
    "USERPROFILE",
    "COMPUTERNAME",
    "NUMBER_OF_PROCESSORS",
    "OS",
    "PROCESSOR_ARCHITECTURE",
    "PROGRAMDATA",
    "PUBLIC",
    "ALLUSERSPROFILE",
    "WINDIR",
    "USERNAME",
];

pub fn is_sensitive_env_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    for pat in [
        "key", "token", "secret", "password", "passwd", "credential", "aws", "azure", "gcp",
        "anthropic", "openai", "api",
    ] {
        if lower.contains(pat) {
            return true;
        }
    }
    false
}

pub fn build_child_env() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (k, v) in std::env::vars() {
        if ENV_ALLOWLIST.iter().any(|a| a.eq_ignore_ascii_case(&k)) && !is_sensitive_env_name(&k) {
            out.push((k, v));
        }
    }
    // Deterministic order for the UTF-16 environment block.
    out.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    out
}

// ---------------------------------------------------------------------------
// Profile validation
// ---------------------------------------------------------------------------

pub fn validate_profile(profile: &CheckProfile, input_paths: &[String]) -> RelayResult<()> {
    if profile.checks.is_empty() {
        return Err(RelayError::validation("profile has no check steps"));
    }
    if profile.checks.len() + profile.prep.len() > 32 {
        return Err(RelayError::validation("profile has too many steps"));
    }
    let mut ids = std::collections::HashSet::new();
    for c in &profile.checks {
        if c.id.is_empty() || c.id.len() > 64 {
            return Err(RelayError::validation("check id must be 1..=64 chars"));
        }
        if !ids.insert(&c.id) {
            return Err(RelayError::validation(format!("duplicate check id: {}", c.id)));
        }
        validate_exe(&c.exe)?;
        validate_cwd_rel(&c.cwd_relative)?;
        if c.timeout_seconds == 0 || c.timeout_seconds > 3600 {
            return Err(RelayError::validation("timeout_seconds must be 1..=3600"));
        }
        validate_args(&c.args)?;
    }
    for p in &profile.prep {
        validate_exe(&p.exe)?;
        validate_cwd_rel(&p.cwd_relative)?;
        if p.timeout_seconds == 0 || p.timeout_seconds > 3600 {
            return Err(RelayError::validation("prep timeout_seconds must be 1..=3600"));
        }
        validate_args(&p.args)?;
    }
    for out in &profile.outputs {
        let norm = out.replace('\\', "/");
        if norm.starts_with('/') || norm.contains("..") || norm.is_empty() {
            return Err(RelayError::validation(format!("bad outputs path: {out}")));
        }
        // outputs must not cover source (input) files.
        for input in input_paths {
            let i_norm = input.replace('\\', "/");
            if i_norm == norm || i_norm.starts_with(&format!("{norm}/")) {
                return Err(RelayError::validation(
                    "outputs paths must not cover source files in the snapshot",
                ));
            }
        }
    }
    Ok(())
}

fn validate_exe(exe: &str) -> RelayResult<()> {
    let p = Path::new(exe);
    if !p.is_absolute() {
        return Err(RelayError::validation(
            "check exe must be an absolute path the user explicitly approved",
        ));
    }
    if exe.contains("..") {
        return Err(RelayError::validation("exe path must not contain .."));
    }
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    if cfg!(windows) {
        match ext.as_str() {
            "exe" => {}
            "bat" | "cmd" | "ps1" | "com" | "scr" => {
                return Err(RelayError::validation(format!(
                    "refusing to execute '{ext}' directly; use a native exe (e.g. pwsh.exe with -File for scripts)"
                )));
            }
            "" => {
                return Err(RelayError::validation("check exe must have an .exe extension on Windows"));
            }
            other => {
                return Err(RelayError::validation(format!(
                    "unsupported executable extension: {other}"
                )));
            }
        }
    }
    Ok(())
}

fn validate_cwd_rel(cwd: &str) -> RelayResult<()> {
    if cwd.is_empty() {
        return Ok(());
    }
    let norm = cwd.replace('\\', "/");
    if norm.starts_with('/') || norm.contains("..") {
        return Err(RelayError::validation("cwd_relative must stay inside the workdir"));
    }
    Ok(())
}

fn validate_args(args: &[String]) -> RelayResult<()> {
    if args.len() > 64 {
        return Err(RelayError::validation("too many argv entries"));
    }
    for a in args {
        if a.len() > 8192 {
            return Err(RelayError::validation("argv entry too long"));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Process runner (Windows Job Objects / Unix process groups)
// ---------------------------------------------------------------------------

pub struct ProcOutcome {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stop_confirmed: bool,
    pub output: Vec<u8>,
    pub output_truncated: bool,
    pub redactions: usize,
}

#[cfg(windows)]
mod winproc {
    use super::*;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT,
    };
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::ReadFile;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
        TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JobObjectExtendedLimitInformation,
    };
    use windows_sys::Win32::System::Pipes::CreatePipe;
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, GetExitCodeProcess, ResumeThread, WaitForSingleObject,
        CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, INFINITE, PROCESS_INFORMATION,
        STARTF_USESTDHANDLES, STARTUPINFOW,
    };

    pub struct Job {
        pub job: HANDLE,
        pub proc: HANDLE,
    }

    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    /// Raw pipe handle wrapper: the handle is used only on its owning thread
    /// for ReadFile; moving it between threads is sound because the kernel
    /// handle value is process-global.
    #[derive(Clone, Copy)]
    pub struct PipeHandle(pub HANDLE);
    unsafe impl Send for PipeHandle {}

    impl Job {
        pub fn close(self) {
            unsafe {
                CloseHandle(self.proc);
                CloseHandle(self.job);
            }
        }

        pub fn kill(&self) {
            unsafe {
                TerminateJobObject(self.job, 1);
            }
        }

        pub fn wait(&self, timeout_ms: u32) -> bool {
            // true = exited
            unsafe { WaitForSingleObject(self.proc, timeout_ms) == 0 }
        }

        pub fn exit_code(&self) -> Option<i32> {
            let mut code: u32 = 259;
            unsafe {
                if GetExitCodeProcess(self.proc, &mut code) != 0 && code != 259 {
                    Some(code as i32)
                } else {
                    None
                }
            }
        }
    }

    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn quote_arg(arg: &str) -> String {
        if !arg.is_empty()
            && !arg.chars().any(|c| c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == '"' || c == '\u{0b}')
        {
            return arg.to_string();
        }
        let mut out = String::from("\"");
        let mut backslashes = 0usize;
        for c in arg.chars() {
            match c {
                '\\' => backslashes += 1,
                '"' => {
                    out.push_str(&"\\".repeat(backslashes * 2 + 1));
                    out.push('"');
                    backslashes = 0;
                }
                c => {
                    out.push_str(&"\\".repeat(backslashes));
                    backslashes = 0;
                    out.push(c);
                }
            }
        }
        out.push_str(&"\\".repeat(backslashes * 2));
        out.push('"');
        out
    }

    fn env_block(env: &[(String, String)]) -> Vec<u16> {
        let mut block: Vec<u16> = Vec::new();
        for (k, v) in env {
            for u in format!("{k}={v}").encode_utf16() {
                block.push(u);
            }
            block.push(0);
        }
        block.push(0);
        block
    }

    pub struct Pipes {
        pub out_read: HANDLE,
        pub err_read: HANDLE,
        pub out_write: HANDLE,
        pub err_write: HANDLE,
    }

    pub fn make_pipes() -> RelayResult<Pipes> {
        let mut sa: SECURITY_ATTRIBUTES = unsafe { std::mem::zeroed() };
        sa.nLength = std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32;
        sa.bInheritHandle = 1;
        let mut out_read: HANDLE = std::ptr::null_mut();
        let mut out_write: HANDLE = std::ptr::null_mut();
        let mut err_read: HANDLE = std::ptr::null_mut();
        let mut err_write: HANDLE = std::ptr::null_mut();
        let ok1 = unsafe { CreatePipe(&mut out_read, &mut out_write, &sa, 0) };
        let ok2 = unsafe { CreatePipe(&mut err_read, &mut err_write, &sa, 0) };
        if ok1 == 0 || ok2 == 0 {
            return Err(RelayError::internal("CreatePipe failed"));
        }
        // Parent-side read ends must NOT be inherited.
        unsafe {
            SetHandleInformation(out_read, HANDLE_FLAG_INHERIT, 0);
            SetHandleInformation(err_read, HANDLE_FLAG_INHERIT, 0);
        }
        Ok(Pipes { out_read, err_read, out_write, err_write })
    }

    pub fn spawn_in_job(
        exe: &str,
        args: &[String],
        cwd: &Path,
        env: &[(String, String)],
        pipes: &Pipes,
    ) -> RelayResult<Job> {
        let exe_w = to_wide(&pathsafe::extended_path(Path::new(exe)).to_string_lossy());
        let mut cmdline = quote_arg(exe);
        for a in args {
            cmdline.push(' ');
            cmdline.push_str(&quote_arg(a));
        }
        let mut cmd_w: Vec<u16> = cmdline.encode_utf16().collect();
        cmd_w.push(0);
        let cwd_w = to_wide(&pathsafe::extended_path(cwd).to_string_lossy());
        let env_w = env_block(env);

        let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        si.dwFlags = STARTF_USESTDHANDLES;
        si.hStdInput = std::ptr::null_mut();
        si.hStdOutput = pipes.out_write;
        si.hStdError = pipes.err_write;
        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

        let ok = unsafe {
            CreateProcessW(
                exe_w.as_ptr(),
                cmd_w.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT,
                env_w.as_ptr() as *const _,
                cwd_w.as_ptr(),
                &mut si,
                &mut pi,
            )
        };
        if ok == 0 {
            return Err(RelayError::internal(format!(
                "CreateProcessW failed (code {})",
                unsafe { windows_sys::Win32::Foundation::GetLastError() }
            )));
        }

        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            unsafe {
                CloseHandle(pi.hThread);
                CloseHandle(pi.hProcess);
            }
            return Err(RelayError::internal("CreateJobObjectW failed"));
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            unsafe {
                TerminateJobObject(job, 1);
                CloseHandle(job);
                CloseHandle(pi.hThread);
                CloseHandle(pi.hProcess);
            }
            return Err(RelayError::internal("SetInformationJobObject failed"));
        }
        let ok = unsafe { AssignProcessToJobObject(job, pi.hProcess) };
        if ok == 0 {
            unsafe {
                TerminateJobObject(job, 1);
                CloseHandle(job);
                CloseHandle(pi.hThread);
                CloseHandle(pi.hProcess);
            }
            return Err(RelayError::internal("AssignProcessToJobObject failed"));
        }
        // Process tree is now fully contained; let it run.
        unsafe {
            ResumeThread(pi.hThread);
            CloseHandle(pi.hThread);
        }
        Ok(Job { job, proc: pi.hProcess })
    }

    pub fn close_read(h: HANDLE) {
        unsafe {
            CloseHandle(h);
        }
    }

    /// Reads a pipe handle to EOF with a capacity cap. Runs on a helper thread.
    pub fn drain_pipe(h: PipeHandle, cap: usize) -> (Vec<u8>, bool) {
        let h: HANDLE = h.0;
        let mut buf: Vec<u8> = Vec::new();
        let mut truncated = false;
        let mut chunk = [0u8; 65536];
        loop {
            let mut n: u32 = 0;
            let ok = unsafe {
                ReadFile(h, chunk.as_mut_ptr(), chunk.len() as u32, &mut n, std::ptr::null_mut())
            };
            if ok == 0 || n == 0 {
                break;
            }
            if buf.len() < cap {
                let take = (cap - buf.len()).min(n as usize);
                buf.extend_from_slice(&chunk[..take]);
                if take < n as usize {
                    truncated = true;
                }
            } else {
                truncated = true;
            }
        }
        (buf, truncated)
    }
}

#[cfg(windows)]
use winproc::{close_read, drain_pipe, make_pipes, spawn_in_job, Job, PipeHandle, Pipes};

fn merge_output(mut out: Vec<u8>, err: Vec<u8>, mut truncated: bool) -> (Vec<u8>, bool) {
    if !err.is_empty() {
        for part in [b"\\n[stderr]\\n".as_slice(), err.as_slice()] {
            let take = OUTPUT_CAP_BYTES.saturating_sub(out.len()).min(part.len());
            out.extend_from_slice(&part[..take]);
            truncated |= take < part.len();
        }
    }
    (out, truncated)
}

#[cfg(unix)]
mod unxproc {
    use super::*;
    use std::io::{self, Read};
    use std::os::{fd::AsRawFd, unix::process::CommandExt};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    fn nonblocking(pipe: &impl AsRawFd) -> io::Result<()> {
        let fd = pipe.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn drain(pipe: &mut impl Read, buf: &mut Vec<u8>, truncated: &mut bool) -> bool {
        let mut chunk = [0; 65536];
        let mut read_data = false;
        // Keep polling the deadline even if the child continuously floods a pipe.
        for _ in 0..8 {
            match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    read_data = true;
                    let take = (OUTPUT_CAP_BYTES - buf.len()).min(n);
                    buf.extend_from_slice(&chunk[..take]);
                    *truncated |= take < n;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        read_data
    }

    pub fn run(
        exe: &str, args: &[String], cwd: &Path, env: &[(String, String)],
        start: Instant, timeout_ms: u64, cancel: &CancelFlag,
    ) -> io::Result<ProcOutcome> {
        let mut child = Command::new(exe)
            .args(args).current_dir(cwd).env_clear().envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped())
            .process_group(0).spawn()?;
        let group = child.id() as i32;
        let kill_group = || unsafe { libc::killpg(group, libc::SIGKILL) };
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        if let Err(e) = nonblocking(&stdout).and_then(|_| nonblocking(&stderr)) {
            kill_group();
            let _ = child.wait();
            return Err(e);
        }
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut truncated = false;
        let deadline = start + Duration::from_millis(timeout_ms);
        let mut timed_out = false;
        let mut killed_at = None;
        let status = loop {
            let reading = drain(&mut stdout, &mut out, &mut truncated)
                | drain(&mut stderr, &mut err, &mut truncated);
            // Observe the exit without reaping, so the process group ID
            // cannot be recycled between observing exit and killing descendants.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let observed = unsafe {
                libc::waitid(libc::P_PID, group as libc::id_t, &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT)
            };
            if observed < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                break None;
            }
            if unsafe { info.si_pid() } != 0 {
                if killed_at.is_none() {
                    kill_group();
                }
                break child.wait().ok();
            }
            let now = Instant::now();
            if let Some(killed) = killed_at {
                if now - killed >= Duration::from_millis(STOP_GRACE_MS) {
                    break None;
                }
            } else if cancel.is_cancelled() || now >= deadline {
                timed_out = !cancel.is_cancelled();
                kill_group();
                killed_at = Some(now);
            }
            if !reading {
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        drain(&mut stdout, &mut out, &mut truncated);
        drain(&mut stderr, &mut err, &mut truncated);
        let (output, output_truncated) = merge_output(out, err, truncated);
        let (redacted, redactions) = secrets::redact_bytes(&output);
        Ok(ProcOutcome {
            exit_code: status.and_then(|s| s.code()),
            timed_out,
            stop_confirmed: status.is_some(),
            output: redacted.into_bytes(),
            output_truncated,
            redactions,
        })
    }

    #[test]
    fn timeout_interrupts_a_child_with_open_output_pipes() {
        let started = Instant::now();
        let result = run(
            "/bin/sh", &["-c".into(), "printf started; /bin/sleep 30 & wait".into()],
            &std::env::temp_dir(), &[], started, 500, &CancelFlag::new(),
        ).unwrap();
        assert!(result.timed_out && result.stop_confirmed);
        assert_eq!(result.output, b"started");
        assert!(started.elapsed() < Duration::from_secs(8));
    }

}

fn run_step(
    exe: &str,
    args: &[String],
    cwd: &Path,
    timeout_ms: u64,
    cancel: &CancelFlag,
) -> ProcOutcome {
    let env = build_child_env();
    let start = std::time::Instant::now();

    #[cfg(windows)]
    {
        let pipes = match make_pipes() {
            Ok(p) => p,
            Err(_) => {
                return ProcOutcome {
                    exit_code: None,
                    timed_out: false,
                    stop_confirmed: true,
                    output: b"[relaycheck] pipe setup failed".to_vec(),
                    output_truncated: false,
                    redactions: 0,
                }
            }
        };
        let job = match spawn_in_job(exe, args, cwd, &env, &pipes) {
            Ok(j) => j,
            Err(e) => {
                return ProcOutcome {
                    exit_code: None,
                    timed_out: false,
                    stop_confirmed: true,
                    output: format!("[relaycheck] spawn failed: {}", e.message).into_bytes(),
                    output_truncated: false,
                    redactions: 0,
                }
            }
        };
        // Drain both pipes on helper threads so the child never blocks on a
        // full pipe, while the parent-side buffer stays bounded.
        let out_read = PipeHandle(pipes.out_read);
        let err_read = PipeHandle(pipes.err_read);
        unsafe {
            close_read(pipes.out_write);
            close_read(pipes.err_write);
        }
        let t_out = std::thread::spawn(move || drain_pipe(out_read, OUTPUT_CAP_BYTES));
        let t_err = std::thread::spawn(move || drain_pipe(err_read, OUTPUT_CAP_BYTES));

        // Poll loop: wait for exit / timeout / cancel.
        let deadline = start + std::time::Duration::from_millis(timeout_ms);
        let mut timed_out = false;
        let mut killed = false;
        loop {
            if job.wait(100) {
                break;
            }
            if cancel.is_cancelled() {
                job.kill();
                killed = true;
                break;
            }
            if std::time::Instant::now() >= deadline {
                job.kill();
                timed_out = true;
                killed = true;
                break;
            }
        }
        // Limited wait for termination to complete.
        let stop_confirmed = if killed { job.wait(STOP_GRACE_MS as u32) } else { true };
        let exit_code = job.exit_code();
        job.close();
        let (out, out_trunc1) = t_out.join().unwrap_or_default();
        let (err, out_trunc2) = t_err.join().unwrap_or_default();
        let (output, output_truncated) = merge_output(out, err, out_trunc1 || out_trunc2);
        let (redacted, redactions) = secrets::redact_bytes(&output);
        ProcOutcome {
            exit_code,
            timed_out,
            stop_confirmed,
            output: redacted.into_bytes(),
            output_truncated,
            redactions,
        }
    }

    #[cfg(unix)]
    {
        unxproc::run(exe, args, cwd, &env, start, timeout_ms, cancel).unwrap_or_else(|e| ProcOutcome {
            exit_code: None,
            timed_out: false,
            stop_confirmed: true,
            output: format!("[relaycheck] spawn failed: {e}").into_bytes(),
            output_truncated: false,
            redactions: 0,
        })
    }
}

// ---------------------------------------------------------------------------
// Workdir materialization + invalidation detection
// ---------------------------------------------------------------------------

pub fn materialize_workdir(
    workdir: &Path,
    files: &[(String, String)],
    blobs: &crate::blobstore::BlobStore,
) -> RelayResult<()> {
    std::fs::create_dir_all(workdir)?;
    for (rel, sha) in files {
        let dest = pathsafe::join_checked(&workdir.to_string_lossy(), rel)?;
        blobs.copy_to(sha, &dest)?;
    }
    Ok(())
}

/// Re-hashes every input file; returns the ones whose bytes changed.
pub fn recheck_inputs(workdir: &Path, files: &[(String, String)]) -> Vec<String> {
    let mut changed = Vec::new();
    for (rel, sha) in files {
        let p = pathsafe::join_checked(&workdir.to_string_lossy(), rel);
        match p.and_then(|p| crate::ids::sha256_file(&pathsafe::extended_path(&p))) {
            Ok(h) if h == *sha => {}
            _ => changed.push(rel.clone()),
        }
    }
    changed
}

/// Walks the workdir and finds files that are neither inputs nor inside an
/// approved outputs path (violations → whole round invalidated, spec R05).
pub fn scan_unauthorized_writes(
    workdir: &Path,
    inputs: &std::collections::HashSet<String>,
    outputs: &[String],
) -> RelayResult<Vec<String>> {
    let mut violations = Vec::new();
    let mut stack = vec![workdir.to_path_buf()];
    let mut visited = 0usize;
    while let Some(dir) = stack.pop() {
        visited += 1;
        if visited > 30_000 {
            break; // bounded walk
        }
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for entry in rd.flatten() {
            let p = entry.path();
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if ft.is_dir() {
                if visited < 2_000 {
                    stack.push(p);
                }
                continue;
            }
            let rel_full = p
                .strip_prefix(workdir)
                .unwrap_or(&p)
                .to_string_lossy()
                .replace('\\', "/");
            if inputs.contains(&rel_full) {
                continue;
            }
            let allowed = outputs.iter().any(|o| {
                let norm = o.replace('\\', "/");
                rel_full == norm || rel_full.starts_with(&format!("{norm}/"))
            });
            if !allowed {
                violations.push(rel_full);
                if violations.len() >= 20 {
                    return Ok(violations);
                }
            }
        }
    }
    Ok(violations)
}

fn summary_text(outcome: &ProcOutcome) -> String {
    let mut s = String::from_utf8_lossy(&outcome.output).into_owned();
    // Bound the stored summary; keep first and last slices for context.
    if s.len() > OUTPUT_CAP_BYTES {
        s = format!("{}…{}", &s[..1024], &s[s.len() - 1024..]);
    }
    s
}

// ---------------------------------------------------------------------------
// The verification round
// ---------------------------------------------------------------------------

pub struct RoundOutput {
    pub record: VerificationRecord,
    pub profile_id: String,
    pub workdir: PathBuf,
}

/// Runs a whole check round for one snapshot in a fresh scratch workdir.
/// `files` is the snapshot's (path, sha256) list. Verdicts come only from
/// exit codes plus unchanged inputs — never from printed PASS text.
#[allow(clippy::too_many_arguments)]
pub fn run_round(
    task_id: &str,
    snapshot_id: &str,
    profile: &CheckProfile,
    profile_id: &str,
    task_revision: i64,
    environment_epoch: i64,
    candidate_epoch: i64,
    files: &[(String, String)],
    blobs: &crate::blobstore::BlobStore,
    scratch_root: &Path,
    cancel: &CancelFlag,
    mut notify: impl FnMut(&str),
) -> RelayResult<RoundOutput> {
    let workdir = scratch_root.join(format!("check-{}", crate::ids::new_token(12)));
    let started = now_ms();
    let mut tool_identity: Vec<String> = Vec::new();
    let mut results: Vec<CheckResult> = Vec::new();

    let outcome = (|| -> RelayResult<VerificationRecord> {
        materialize_workdir(&workdir, files, blobs)?;
        let input_set: std::collections::HashSet<String> =
            files.iter().map(|(p, _)| p.replace('\\', "/")).collect();
        let round_deadline = std::time::Instant::now()
            + std::time::Duration::from_secs(ROUND_TIMEOUT_SECONDS);

        // Prep steps (dependency install). Missing deps are blocked, not failed.
        for step in &profile.prep {
            if cancel.is_cancelled() {
                return Ok(finish_record(
                    task_id, snapshot_id, profile, profile_id, task_revision,
                    environment_epoch, candidate_epoch, &tool_identity, results, started,
                    Some("interrupted"),
                ));
            }
            let exe_path = Path::new(&step.exe);
            if !exe_path.exists() {
                results.push(CheckResult {
                    check_id: format!("prep:{}", step.id),
                    status: "blocked".into(),
                    exit_code: None,
                    summary: String::new(),
                    truncated: false,
                    redactions: 0,
                    duration_ms: 0,
                    detail: format!("prep exe not found: {}", exe_name(&step.exe)),
                });
                return Ok(finish_record(
                    task_id, snapshot_id, profile, profile_id, task_revision,
                    environment_epoch, candidate_epoch, &tool_identity, results, started, None,
                ));
            }
            record_tool_identity(&mut tool_identity, &step.exe);
            notify(&format!("prep:{}", step.id));
            let cwd = join_cwd(&workdir, &step.cwd_relative)?;
            let remaining = round_deadline
                .saturating_duration_since(std::time::Instant::now())
                .as_millis() as u64;
            let timeout = (step.timeout_seconds * 1000).min(remaining);
            let oc = run_step(&step.exe, &step.args, &cwd, timeout, cancel);
            if oc.timed_out || cancel.is_cancelled() {
                if !oc.stop_confirmed {
                    return Err(RelayError::new(
                        Code::EStopUnconfirmed,
                        "prep process did not confirm termination",
                    ));
                }
                results.push(CheckResult {
                    check_id: format!("prep:{}", step.id),
                    status: "interrupted".into(),
                    exit_code: oc.exit_code,
                    summary: summary_text(&oc),
                    truncated: oc.output_truncated,
                    redactions: oc.redactions,
                    duration_ms: 0,
                    detail: if oc.timed_out { "timeout".into() } else { "cancelled".into() },
                });
                return Ok(finish_record(
                    task_id, snapshot_id, profile, profile_id, task_revision,
                    environment_epoch, candidate_epoch, &tool_identity, results, started,
                    Some("interrupted"),
                ));
            }
        }

        // Baseline input hashes AFTER prep; checks must not modify inputs.
        let changed = recheck_inputs(&workdir, files);
        if !changed.is_empty() {
            return Ok(finish_record(
                task_id, snapshot_id, profile, profile_id, task_revision,
                environment_epoch, candidate_epoch, &tool_identity,
                vec![CheckResult {
                    check_id: "input-integrity".into(),
                    status: "invalidated".into(),
                    exit_code: None,
                    summary: String::new(),
                    truncated: false,
                    redactions: 0,
                    duration_ms: 0,
                    detail: format!("files modified during prep: {}", changed.join(", ")),
                }],
                started, None,
            ));
        }

        // Check steps, in profile order.
        for step in &profile.checks {
            if cancel.is_cancelled() {
                return Ok(finish_record(
                    task_id, snapshot_id, profile, profile_id, task_revision,
                    environment_epoch, candidate_epoch, &tool_identity, results, started,
                    Some("interrupted"),
                ));
            }
            let exe_path = Path::new(&step.exe);
            if !exe_path.exists() {
                results.push(CheckResult {
                    check_id: step.id.clone(),
                    status: "blocked".into(),
                    exit_code: None,
                    summary: String::new(),
                    truncated: false,
                    redactions: 0,
                    duration_ms: 0,
                    detail: format!("missing dependency: {}", exe_name(&step.exe)),
                });
                // Blocked required check → whole round blocked; stop here.
                return Ok(finish_record(
                    task_id, snapshot_id, profile, profile_id, task_revision,
                    environment_epoch, candidate_epoch, &tool_identity, results, started, None,
                ));
            }
            record_tool_identity(&mut tool_identity, &step.exe);
            notify(&format!("check:{}", step.id));
            let step_start = std::time::Instant::now();
            let cwd = join_cwd(&workdir, &step.cwd_relative)?;
            let remaining = round_deadline
                .saturating_duration_since(std::time::Instant::now())
                .as_millis() as u64;
            if remaining == 0 {
                results.push(CheckResult {
                    check_id: step.id.clone(),
                    status: "interrupted".into(),
                    exit_code: None,
                    summary: String::new(),
                    truncated: false,
                    redactions: 0,
                    duration_ms: 0,
                    detail: "round time budget exhausted".into(),
                });
                return Ok(finish_record(
                    task_id, snapshot_id, profile, profile_id, task_revision,
                    environment_epoch, candidate_epoch, &tool_identity, results, started,
                    Some("interrupted"),
                ));
            }
            let timeout = (step.timeout_seconds * 1000).min(remaining);
            let oc = run_step(&step.exe, &step.args, &cwd, timeout, cancel);
            let duration = step_start.elapsed().as_millis() as i64;

            if !oc.stop_confirmed {
                return Err(RelayError::new(
                    Code::EStopUnconfirmed,
                    "check process did not confirm termination; write lock retained",
                ));
            }

            // Inputs must be byte-identical after every step, exit code
            // notwithstanding (spec R05: 源/测试/lock变化则整轮invalidated).
            let changed = recheck_inputs(&workdir, files);
            if !changed.is_empty() {
                results.push(CheckResult {
                    check_id: step.id.clone(),
                    status: "invalidated".into(),
                    exit_code: oc.exit_code,
                    summary: summary_text(&oc),
                    truncated: oc.output_truncated,
                    redactions: oc.redactions,
                    duration_ms: duration,
                    detail: format!("input files modified by check: {}", changed.join(", ")),
                });
                return Ok(finish_record(
                    task_id, snapshot_id, profile, profile_id, task_revision,
                    environment_epoch, candidate_epoch, &tool_identity, results, started, None,
                ));
            }

            let status = if oc.timed_out {
                "interrupted"
            } else if cancel.is_cancelled() {
                "interrupted"
            } else if oc.exit_code == Some(0) {
                "passed"
            } else {
                "failed"
            };
            results.push(CheckResult {
                check_id: step.id.clone(),
                status: status.into(),
                exit_code: oc.exit_code,
                summary: summary_text(&oc),
                truncated: oc.output_truncated,
                redactions: oc.redactions,
                duration_ms: duration,
                detail: if oc.timed_out {
                    "timeout".into()
                } else if cancel.is_cancelled() {
                    "cancelled".into()
                } else {
                    String::new()
                },
            });
            if status == "interrupted" {
                return Ok(finish_record(
                    task_id, snapshot_id, profile, profile_id, task_revision,
                    environment_epoch, candidate_epoch, &tool_identity, results, started,
                    Some("interrupted"),
                ));
            }
        }

        // Unauthorized writes outside approved outputs invalidate the round
        // even when every exit code was 0 (spec R05: 只写批准outputs允许).
        let violations = scan_unauthorized_writes(&workdir, &input_set, &profile.outputs)?;
        if !violations.is_empty() {
            results.push(CheckResult {
                check_id: "output-scope".into(),
                status: "invalidated".into(),
                exit_code: None,
                summary: String::new(),
                truncated: false,
                redactions: 0,
                duration_ms: 0,
                detail: format!("writes outside approved outputs: {}", violations.join(", ")),
            });
            return Ok(finish_record(
                task_id, snapshot_id, profile, profile_id, task_revision,
                environment_epoch, candidate_epoch, &tool_identity, results, started, None,
            ));
        }

        Ok(finish_record(
            task_id, snapshot_id, profile, profile_id, task_revision, environment_epoch,
            candidate_epoch, &tool_identity, results, started, None,
        ))
    })();

    let record = match outcome {
        Ok(r) => r,
        Err(e) => {
            // E_STOP_UNCONFIRMED propagates; cleanup what we can.
            cleanup_workdir(&workdir);
            return Err(e);
        }
    };
    cleanup_workdir(&workdir);
    Ok(RoundOutput { record, profile_id: profile_id.to_string(), workdir })
}

fn finish_record(
    task_id: &str,
    snapshot_id: &str,
    profile: &CheckProfile,
    _profile_id: &str,
    task_revision: i64,
    environment_epoch: i64,
    candidate_epoch: i64,
    tool_identity: &[String],
    mut results: Vec<CheckResult>,
    started: i64,
    force_status: Option<&str>,
) -> VerificationRecord {
    // Fill results for checks that never ran (blocked earlier / interrupted).
    for step in &profile.checks {
        if !results.iter().any(|r| r.check_id == step.id) {
            results.push(CheckResult {
                check_id: step.id.clone(),
                status: "pending".into(),
                exit_code: None,
                summary: String::new(),
                truncated: false,
                redactions: 0,
                duration_ms: 0,
                detail: "not run".into(),
            });
        }
    }
    results.sort_by(|a, b| a.check_id.cmp(&b.check_id));
    let required: Vec<&crate::model::CheckStep> =
        profile.checks.iter().filter(|c| c.required).collect();
    // Round verdict priority: any invalidation poisons the whole round
    // (output-scope / input-integrity pseudo-checks included, spec R05);
    // then a blocked required step; then a failed required check; then all
    // required passed; anything incomplete is interrupted.
    let status_of = |id: &str| results.iter().find(|r| r.check_id == id).map(|r| r.status.as_str());
    let round_status: String = if let Some(f) = force_status {
        f.to_string()
    } else if results.iter().any(|r| r.status == "invalidated") {
        "invalidated".to_string()
    } else if !required.is_empty() && required.iter().any(|c| status_of(&c.id) == Some("blocked")) {
        "blocked".to_string()
    } else if !required.is_empty() && required.iter().any(|c| status_of(&c.id) == Some("failed")) {
        "failed".to_string()
    } else if !required.is_empty() && required.iter().all(|c| status_of(&c.id) == Some("passed")) {
        "passed".to_string()
    } else if required.is_empty() {
        // No required checks: the round can hand off but never gates
        // ready_for_review/acceptance (spec §3).
        "passed".to_string()
    } else {
        "interrupted".to_string()
    };
    VerificationRecord {
        id: crate::ids::new_uuid(),
        task_id: task_id.to_string(),
        snapshot_id: snapshot_id.to_string(),
        profile_hash: profile.hash.clone(),
        task_revision,
        environment_epoch,
        candidate_epoch,
        tool_identity: tool_identity.to_vec(),
        results,
        round_status: round_status.to_string(),
        created_at_ms: started,
    }
}

fn record_tool_identity(tool_identity: &mut Vec<String>, exe: &str) {
    let key = exe.to_string();
    if tool_identity.iter().any(|t| t.starts_with(&key)) {
        return;
    }
    let md = std::fs::metadata(exe).ok();
    let size = md.as_ref().map(|m| m.len()).unwrap_or(0);
    let mtime = md
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    tool_identity.push(format!("{key} ({size} bytes, mtime {mtime})"));
}

fn exe_name(exe: &str) -> String {
    Path::new(exe)
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| exe.to_string())
}

fn join_cwd(workdir: &Path, cwd_relative: &str) -> RelayResult<PathBuf> {
    if cwd_relative.is_empty() {
        return Ok(workdir.to_path_buf());
    }
    pathsafe::join_checked(&workdir.to_string_lossy(), cwd_relative)
}

pub fn cleanup_workdir(workdir: &Path) {
    // Bounded, best-effort cleanup of our own scratch dir.
    for _ in 0..3 {
        if std::fs::remove_dir_all(workdir).is_ok() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

pub fn now_ms() -> i64 {
    crate::ids::now_ms()
}
