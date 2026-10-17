//! The Live Map's Here button asks for this Mac's position with `navigator.geolocation`.
//!
//! WebKit refuses such requests unless the app's web view delegate answers them, and wry's
//! delegate has no answer, so the page said location was off. WebKit asks through a private
//! delegate method (public only on iOS); this adds it to the Live Map's delegate. It says yes
//! only for the site itself (`site::shares_location`), and macOS still asks the owner once
//! before any position is given (Info.plist has the reason).

use crate::site;
use block2::Block;
use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
use objc2::{msg_send, sel};
use std::ffi::{c_char, c_void, CStr};

/// WKPermissionDecision
const GRANT: isize = 1;
const DENY: isize = 2;

/// Teach the web view's delegate to answer location requests. Call on the main thread.
pub fn allow_location(webview: *mut c_void) {
    let webview = webview as *mut AnyObject;
    if webview.is_null() {
        return;
    }
    unsafe {
        let delegate: *mut AnyObject = msg_send![webview, UIDelegate];
        if delegate.is_null() {
            log::warn!("Live Map: no web view delegate, Here will not find this Mac");
            return;
        }
        let class = (*delegate).class() as *const AnyClass as *mut AnyClass;
        let imp: Imp = std::mem::transmute(
            request_location as unsafe extern "C-unwind" fn(_, _, _, _, _, _),
        );
        // false when the class has it already: the dashboard and the map share the class
        let _ = objc2::ffi::class_addMethod(
            class,
            sel!(_webView:requestGeolocationPermissionForOrigin:initiatedByFrame:decisionHandler:),
            imp,
            c"v@:@@@@?".as_ptr(),
        );
    }
}

/// `-[WKUIDelegatePrivate _webView:requestGeolocationPermissionForOrigin:initiatedByFrame:decisionHandler:]`
unsafe extern "C-unwind" fn request_location(
    _this: *mut AnyObject,
    _cmd: Sel,
    _webview: *mut AnyObject,
    origin: *mut AnyObject,
    _frame: *mut AnyObject,
    handler: *mut Block<dyn Fn(isize)>,
) {
    let allowed = !origin.is_null() && {
        let protocol = string(msg_send![origin, protocol]);
        let host = string(msg_send![origin, host]);
        let port: isize = msg_send![origin, port];
        site::shares_location(&protocol, &host, port as i64)
    };
    if !allowed {
        log::info!("Live Map: a location request from another site was refused");
    }
    if let Some(h) = handler.as_ref() {
        h.call((if allowed { GRANT } else { DENY },));
    }
}

/// An NSString as a Rust string ("" for nil).
unsafe fn string(s: *mut AnyObject) -> String {
    if s.is_null() {
        return String::new();
    }
    let p: *const c_char = msg_send![s, UTF8String];
    if p.is_null() { String::new() } else { CStr::from_ptr(p).to_string_lossy().into_owned() }
}
