//! Watches a folder for changes with macOS FSEvents (what the watcher uses on macOS),
//! through our own bindings to CoreServices. Events are delivered on a background thread
//! running a CFRunLoop; `Watcher::poll` hands out the changed paths and the waker tells the UI
//! to look.

use std::ffi::{c_char, c_void, CStr, CString};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

pub type Waker = Arc<dyn Fn() + Send + Sync>;

type CFRef = *const c_void;

#[repr(C)]
struct FSEventStreamContext {
    version: isize,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
}

type Callback = extern "C" fn(stream: CFRef, info: *mut c_void, count: usize, paths: *mut c_void, flags: *const u32, ids: *const u64);

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn FSEventStreamCreate(
        allocator: CFRef,
        callback: Callback,
        context: *const FSEventStreamContext,
        paths: CFRef,
        since_when: u64,
        latency: f64,
        flags: u32,
    ) -> CFRef;
    fn FSEventStreamScheduleWithRunLoop(stream: CFRef, run_loop: CFRef, mode: CFRef);
    fn FSEventStreamStart(stream: CFRef) -> u8;
    fn FSEventStreamStop(stream: CFRef);
    fn FSEventStreamInvalidate(stream: CFRef);
    fn FSEventStreamRelease(stream: CFRef);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFRunLoopDefaultMode: CFRef;
    static kCFTypeArrayCallBacks: c_void;
    fn CFStringCreateWithCString(allocator: CFRef, s: *const c_char, encoding: u32) -> CFRef;
    fn CFArrayCreate(allocator: CFRef, values: *const CFRef, count: isize, callbacks: *const c_void) -> CFRef;
    fn CFRelease(cf: CFRef);
    fn CFRunLoopGetCurrent() -> CFRef;
    fn CFRunLoopRunInMode(mode: CFRef, seconds: f64, return_after_source_handled: u8) -> i32;
    fn CFRunLoopStop(run_loop: CFRef);
}

const UTF8: u32 = 0x0800_0100;
const SINCE_NOW: u64 = u64::MAX;
/// kFSEventStreamCreateFlagNoDefer | kFSEventStreamCreateFlagFileEvents.
const FLAGS: u32 = 0x02 | 0x10;
/// Seconds FSEvents gathers changes before reporting them.
const LATENCY: f64 = 0.1;

struct Sink {
    tx: Sender<PathBuf>,
    waker: Waker,
}

extern "C" fn on_events(_stream: CFRef, info: *mut c_void, count: usize, paths: *mut c_void, _flags: *const u32, _ids: *const u64) {
    // SAFETY: `info` is the `Sink` the thread owns for the stream's lifetime, and without
    // kFSEventStreamCreateFlagUseCFTypes `paths` is an array of `count` C strings.
    let sink = unsafe { &*(info as *const Sink) };
    let paths = paths as *const *const c_char;
    for i in 0..count {
        let path = unsafe { CStr::from_ptr(*paths.add(i)) };
        let _ = sink.tx.send(PathBuf::from(path.to_string_lossy().into_owned()));
    }
    (sink.waker)();
}

/// The watching thread's run loop, stopped on drop.
struct RunLoop(CFRef);
// SAFETY: CFRunLoopStop may be called from any thread.
unsafe impl Send for RunLoop {}

pub struct Watcher {
    rx: Receiver<PathBuf>,
    run_loop: RunLoop,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Watcher {
    /// Starts watching `root` and everything under it.
    pub fn new(root: &Path, waker: Waker) -> io::Result<Self> {
        let root = CString::new(root.to_string_lossy().into_owned()).map_err(|_| io::Error::other("path contains NUL"))?;
        let (tx, rx) = channel();
        let (ready_tx, ready_rx) = channel::<Option<RunLoop>>();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = std::thread::Builder::new().name("fswatch".into()).spawn(move || {
            let sink = Box::new(Sink { tx, waker });
            let context = FSEventStreamContext {
                version: 0,
                info: &*sink as *const Sink as *mut c_void,
                retain: std::ptr::null(),
                release: std::ptr::null(),
                copy_description: std::ptr::null(),
            };
            // SAFETY: plain CoreFoundation/CoreServices calls; every object created here is
            // released before the thread ends, and `sink` outlives the stream.
            unsafe {
                let path = CFStringCreateWithCString(std::ptr::null(), root.as_ptr(), UTF8);
                let paths = CFArrayCreate(std::ptr::null(), &path, 1, &kCFTypeArrayCallBacks);
                let stream = FSEventStreamCreate(std::ptr::null(), on_events, &context, paths, SINCE_NOW, LATENCY, FLAGS);
                CFRelease(paths);
                CFRelease(path);
                if stream.is_null() {
                    let _ = ready_tx.send(None);
                    return;
                }
                let run_loop = CFRunLoopGetCurrent();
                FSEventStreamScheduleWithRunLoop(stream, run_loop, kCFRunLoopDefaultMode);
                if FSEventStreamStart(stream) == 0 {
                    FSEventStreamInvalidate(stream);
                    FSEventStreamRelease(stream);
                    let _ = ready_tx.send(None);
                    return;
                }
                let _ = ready_tx.send(Some(RunLoop(run_loop)));
                // In slices, so a stop requested before the loop started isn't missed.
                while !stopping.load(Ordering::Relaxed) {
                    CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.5, 0);
                }
                FSEventStreamStop(stream);
                FSEventStreamInvalidate(stream);
                FSEventStreamRelease(stream);
            }
            drop(sink);
        })?;
        match ready_rx.recv() {
            Ok(Some(run_loop)) => Ok(Self { rx, run_loop, stop, thread: Some(thread) }),
            _ => {
                let _ = thread.join();
                Err(io::Error::other("couldn't start watching"))
            }
        }
    }

    /// The paths that changed since the last call (files and folders, possibly repeated).
    pub fn poll(&self) -> Vec<PathBuf> {
        self.rx.try_iter().collect()
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // SAFETY: the run loop belongs to our thread, which is still running it.
        unsafe { CFRunLoopStop(self.run_loop.0) };
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn reports_created_files() {
        let dir = std::env::temp_dir().join(format!("fswatch-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let w = Watcher::new(&dir, Arc::new(|| {})).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        std::fs::write(dir.join("new.txt"), "hi").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = Vec::new();
        while Instant::now() < deadline && !seen.iter().any(|p: &PathBuf| p.ends_with("new.txt")) {
            std::thread::sleep(Duration::from_millis(50));
            seen.extend(w.poll());
        }
        drop(w);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(seen.iter().any(|p| p.ends_with("new.txt")), "saw {seen:?}");
    }
}
