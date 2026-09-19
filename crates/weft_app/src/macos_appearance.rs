//! v1.11.12 (PLAN_v11112 M-D): pure decision core for the macOS system
//! appearance query, extracted from `macos_system.rs` (which sits AT the
//! 800-line architecture ceiling — this sibling module follows the
//! set_dock_badge → macos_notifications precedent) so the failure-path
//! semantics are unit-testable without a live AppKit session.

use objc2::ffi::objc_class;
use objc2::runtime::AnyObject;

/// Decide "is the system appearance dark" from already-resolved ObjC parts.
///
/// Pointer-parameterized pure function (the production wrapper in
/// `macos_system::system_appearance_is_dark` feeds it real pointers; tests
/// feed nulls). Semantics (v1.11.12 D-d — unified with the panic fallback):
///
/// - ANY null infrastructure pointer (`defaults_cls`, `str_cls`, `defaults`,
///   `key_ns`, or the `UTF8String` result) means the QUERY MECHANISM is
///   broken → default to **dark** (terminal convention, and the same safe
///   default the `guarded_unwind` panic fallback uses). These paths
///   previously returned `false` (light), contradicting the panic path.
/// - A legal nil `stringForKey:` result is NOT a failure: the
///   `AppleInterfaceStyle` key only exists in Dark mode, so nil = Light →
///   `false`.
///
/// The non-null pointers are only ever checked for null-ness or messaged
/// via `UTF8String` on the REAL `value_ns` object — never dereferenced
/// otherwise.
///
/// # Safety
/// Each pointer must be either null or a valid Objective-C object/class of
/// the expected kind (the production wrapper's `msg_send!` chain guarantees
/// this); only `value_ns` receives a message.
pub(crate) unsafe fn appearance_from_parts(
    defaults_cls: *const objc_class,
    str_cls: *const objc_class,
    defaults: *mut AnyObject,
    key_ns: *mut AnyObject,
    value_ns: *mut AnyObject,
) -> bool {
    // Broken query mechanism → default dark (v1.11.12 D-d).
    if defaults_cls.is_null() || str_cls.is_null() || defaults.is_null() || key_ns.is_null() {
        return true;
    }
    // Legal nil: AppleInterfaceStyle absent → Light mode (macOS default).
    if value_ns.is_null() {
        return false;
    }
    // VULN-005: UTF8String on a corrupted/non-string object can raise an
    // ObjC assertion — degrade to the same "broken query → default dark"
    // semantics as the null checks above.
    let c_str: *const i8 = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        objc2::msg_send![value_ns, UTF8String]
    })) {
        Ok(c_str) => c_str,
        Err(_) => return true,
    };
    if c_str.is_null() {
        // UTF8String failing is a broken query too → default dark.
        return true;
    }
    let raw = std::ffi::CStr::from_ptr(c_str);
    let s = raw.to_str().unwrap_or("").trim().to_ascii_lowercase();
    s == "dark"
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOME_CLASS: *const objc_class = 8usize as *const objc_class;
    const SOME_OBJECT: *mut AnyObject = 16usize as *mut AnyObject;

    /// Every null-infrastructure combination must default to dark (true) —
    /// the v1.11.12 D-d unification (previously false, contradicting the
    /// panic fallback). The pointers below are never dereferenced when any
    /// of them is null, so fabricated non-null values are safe here.
    #[test]
    fn null_infrastructure_defaults_to_dark() {
        unsafe {
            // Each of the four infrastructure pointers null in turn.
            assert!(appearance_from_parts(
                std::ptr::null(),
                SOME_CLASS,
                SOME_OBJECT,
                SOME_OBJECT,
                std::ptr::null_mut(),
            ));
            assert!(appearance_from_parts(
                SOME_CLASS,
                std::ptr::null(),
                SOME_OBJECT,
                SOME_OBJECT,
                std::ptr::null_mut(),
            ));
            assert!(appearance_from_parts(
                SOME_CLASS,
                SOME_CLASS,
                std::ptr::null_mut(),
                SOME_OBJECT,
                std::ptr::null_mut(),
            ));
            assert!(appearance_from_parts(
                SOME_CLASS,
                SOME_CLASS,
                SOME_OBJECT,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ));
            // All null at once.
            assert!(appearance_from_parts(
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ));
        }
    }

    /// A legal nil `stringForKey:` result (key absent) is Light — NOT a
    /// failure — when the surrounding infrastructure resolved fine.
    #[test]
    fn nil_value_is_legal_light() {
        unsafe {
            assert!(!appearance_from_parts(
                SOME_CLASS,
                SOME_CLASS,
                SOME_OBJECT,
                SOME_OBJECT,
                std::ptr::null_mut(),
            ));
        }
    }

    /// Equivalence with the production wrapper's real path: a genuine
    /// NSString carrying the style value decides dark/light/unknown.
    #[test]
    fn real_value_strings_decide_like_the_wrapper() {
        use objc2::rc::Retained;
        use objc2_foundation::NSString;
        let check = |value: &str| -> bool {
            let retained = NSString::from_str(value);
            let value_ns: *mut AnyObject = Retained::into_raw(retained) as *mut AnyObject;
            // SAFETY: value_ns is a real NSString; the other pointers are
            // never dereferenced (non-null checks only).
            let result = unsafe {
                appearance_from_parts(SOME_CLASS, SOME_CLASS, SOME_OBJECT, SOME_OBJECT, value_ns)
            };
            drop(unsafe { Retained::from_raw(value_ns as *mut NSString) });
            result
        };
        assert!(check("Dark"));
        assert!(check("DARK "));
        assert!(!check("Light"));
        assert!(!check("Auto"));
    }
}
