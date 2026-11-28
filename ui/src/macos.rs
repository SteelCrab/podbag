//! macOS: what winit does not report. A click on the Dock icon of a running app
//! ("reopen") reaches the app delegate only, so the delegate winit installs gets
//! `applicationShouldHandleReopen:hasVisibleWindows:`, which calls back here.

use std::ffi::CStr;
use std::sync::Mutex;

use objc2::encode::Encode;
use objc2::runtime::{AnyClass, AnyObject, Bool, Imp, Sel};
use objc2::sel;

type Callback = Box<dyn Fn() + Send>;

static REOPEN: Mutex<Option<Callback>> = Mutex::new(None);

extern "C-unwind" fn should_handle_reopen(
    _this: *mut AnyObject,
    _cmd: Sel,
    _app: *mut AnyObject,
    _has_visible_windows: Bool,
) -> Bool {
    if let Some(f) = REOPEN.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
        f();
    }
    // Handled: AppKit does nothing more.
    Bool::NO
}

/// Call `f` on each reopen. Call it after the event loop exists (winit has made its
/// delegate class then).
pub(crate) fn on_reopen(f: impl Fn() + Send + 'static) {
    *REOPEN.lock().unwrap_or_else(|p| p.into_inner()) = Some(Box::new(f));
    const DELEGATE: &CStr = c"WinitApplicationDelegate";
    let Some(class) = AnyClass::get(DELEGATE) else {
        return;
    };
    let types = format!(
        "{}{}{}{}{}\0",
        Bool::ENCODING,
        <*mut AnyObject>::ENCODING,
        Sel::ENCODING,
        <*mut AnyObject>::ENCODING,
        Bool::ENCODING
    );
    type Method = extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject, Bool) -> Bool;
    let method: Method = should_handle_reopen;
    // SAFETY: the types string describes `method`'s signature; adding a method to a
    // registered class is allowed, and it fails harmlessly if the class has one.
    unsafe {
        let imp: Imp = std::mem::transmute::<Method, Imp>(method);
        objc2::ffi::class_addMethod(
            class as *const AnyClass as *mut AnyClass,
            sel!(applicationShouldHandleReopen:hasVisibleWindows:),
            imp,
            types.as_ptr().cast(),
        );
    }
}
