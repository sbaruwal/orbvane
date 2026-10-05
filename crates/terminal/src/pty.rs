//! Pseudo-terminal: spawns a shell on a new PTY with `forkpty`.

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::Path;

pub struct Pty {
    master: File,
    pid: libc::pid_t,
}

fn winsize(cols: u16, rows: u16) -> libc::winsize {
    libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 }
}

fn cstring(s: &str) -> io::Result<CString> {
    CString::new(s).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "string contains NUL"))
}

impl Pty {
    /// Starts `program` with `args` in `cwd` on a new PTY. `env` entries override the
    /// inherited environment.
    pub fn spawn(program: &str, args: &[&str], cwd: &Path, env: &[(&str, &str)], cols: u16, rows: u16) -> io::Result<Self> {
        // Build everything before forking: only async-signal-safe calls are allowed in
        // the child of a multithreaded process.
        let prog = cstring(program)?;
        let argv_owned: Vec<CString> = args.iter().map(|a| cstring(a)).collect::<io::Result<_>>()?;
        let mut argv: Vec<*const libc::c_char> = argv_owned.iter().map(|a| a.as_ptr()).collect();
        argv.push(std::ptr::null());

        let mut vars: Vec<(String, String)> = std::env::vars().filter(|(k, _)| !env.iter().any(|(ek, _)| ek == k)).collect();
        vars.extend(env.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        let env_owned: Vec<CString> = vars.iter().filter_map(|(k, v)| CString::new(format!("{k}={v}")).ok()).collect();
        let mut envp: Vec<*const libc::c_char> = env_owned.iter().map(|e| e.as_ptr()).collect();
        envp.push(std::ptr::null());

        let dir = cstring(&cwd.to_string_lossy())?;
        let mut ws = winsize(cols, rows);
        let mut master: libc::c_int = -1;

        // SAFETY: forkpty is called with valid pointers; the child branch only makes
        // async-signal-safe calls (chdir, execve, _exit) on data prepared above.
        let pid = unsafe { libc::forkpty(&mut master, std::ptr::null_mut(), std::ptr::null_mut(), &mut ws) };
        if pid < 0 {
            return Err(io::Error::last_os_error());
        }
        if pid == 0 {
            unsafe {
                libc::chdir(dir.as_ptr());
                libc::execve(prog.as_ptr(), argv.as_ptr(), envp.as_ptr());
                libc::_exit(127);
            }
        }
        // SAFETY: forkpty returned a new, owned master descriptor.
        let master = unsafe { File::from_raw_fd(master) };
        unsafe {
            libc::fcntl(master.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
        }
        Ok(Self { master, pid })
    }

    pub fn reader(&self) -> io::Result<File> {
        self.master.try_clone()
    }

    pub fn writer(&self) -> &File {
        &self.master
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let ws = winsize(cols, rows);
        // SAFETY: TIOCSWINSZ reads a winsize from a valid pointer.
        unsafe {
            libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &ws);
        }
    }

    pub fn pid(&self) -> libc::pid_t {
        self.pid
    }

    /// The name of the program in the foreground of the terminal ("zsh", "vim", "cargo").
    pub fn foreground_name(&self) -> Option<String> {
        // SAFETY: tcgetpgrp only reads the descriptor; proc_name writes at most `buf.len()` bytes.
        let pgid = unsafe { libc::tcgetpgrp(self.master.as_raw_fd()) };
        let pid = if pgid > 0 { pgid } else { self.pid };
        let mut buf = [0u8; 256];
        let n = unsafe { libc::proc_name(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
        (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
    }

    /// The shell's current directory.
    pub fn cwd(&self) -> Option<std::path::PathBuf> {
        // SAFETY: proc_pidinfo fills a proc_vnodepathinfo of the size we pass.
        let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
        let n = unsafe { libc::proc_pidinfo(self.pid, libc::PROC_PIDVNODEPATHINFO, 0, (&mut info as *mut libc::proc_vnodepathinfo).cast(), size) };
        if n != size {
            return None;
        }
        // libc declares the MAXPATHLEN buffer as 32 chunks of 32.
        let path = info.pvi_cdir.vip_path.iter().flatten();
        let bytes: Vec<u8> = path.take_while(|&&c| c != 0).map(|&c| c as u8).collect();
        (!bytes.is_empty()).then(|| std::path::PathBuf::from(String::from_utf8_lossy(&bytes).into_owned()))
    }

    /// Sends SIGHUP to the shell's process group, like closing a terminal window.
    pub fn hangup(&self) {
        unsafe {
            libc::kill(-self.pid, libc::SIGHUP);
            libc::kill(self.pid, libc::SIGHUP);
        }
    }
}

/// Waits for `pid` to exit and returns its exit code (or 128 + signal).
pub fn wait(pid: libc::pid_t) -> Option<i32> {
    let mut status = 0;
    // SAFETY: waitpid writes the status to a valid pointer.
    let r = unsafe { libc::waitpid(pid, &mut status, 0) };
    if r != pid {
        return None;
    }
    if libc::WIFEXITED(status) {
        Some(libc::WEXITSTATUS(status))
    } else if libc::WIFSIGNALED(status) {
        Some(128 + libc::WTERMSIG(status))
    } else {
        None
    }
}
