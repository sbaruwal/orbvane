//! Moving files to the Trash with `-[NSFileManager trashItemAtURL:resultingItemURL:error:]`
//!, through our own Objective-C calls.

use std::ffi::{CStr, CString};
use std::path::Path;

use objc2::runtime::{AnyObject, Bool};
use objc2::{class, msg_send};

/// Moves `path` (a file or a folder) to the Trash.
pub fn move_to_trash(path: &Path) -> Result<(), String> {
    let c = CString::new(path.to_string_lossy().as_bytes()).map_err(|e| e.to_string())?;
    objc2::rc::autoreleasepool(|_| {
        // SAFETY: plain Foundation calls with valid arguments; the returned objects are
        // autoreleased and only used inside this pool.
        unsafe {
            let s: *mut AnyObject = msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()];
            let url: *mut AnyObject = msg_send![class!(NSURL), fileURLWithPath: s];
            let fm: *mut AnyObject = msg_send![class!(NSFileManager), defaultManager];
            let mut error: *mut AnyObject = std::ptr::null_mut();
            let ok: Bool = msg_send![fm, trashItemAtURL: url, resultingItemURL: std::ptr::null_mut::<*mut AnyObject>(), error: &mut error];
            if ok.as_bool() {
                return Ok(());
            }
            if error.is_null() {
                return Err("Couldn't move the item to the Trash.".into());
            }
            let desc: *mut AnyObject = msg_send![error, localizedDescription];
            let utf8: *const std::ffi::c_char = msg_send![desc, UTF8String];
            Err(if utf8.is_null() { "Couldn't move the item to the Trash.".into() } else { CStr::from_ptr(utf8).to_string_lossy().into_owned() })
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trashes_a_file() {
        let dir = std::env::temp_dir().join(format!("orbvane-trash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("scratch.txt");
        std::fs::write(&file, "x").unwrap();
        move_to_trash(&file).unwrap();
        assert!(!file.exists());
        assert!(move_to_trash(&dir.join("missing.txt")).is_err());
        let _ = std::fs::remove_dir(&dir);
    }
}
