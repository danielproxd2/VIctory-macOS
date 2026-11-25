//! System dialogs a title opens (XGameUi). A profile card has nothing to
//! show and closes at once. Text entry opens a small host dialog (title,
//! description, default text, the length limit) on a thread of its own and
//! completes the call with what the user typed, or as cancelled (`E_ABORT`)
//! when the dialog is dismissed or cannot open. A message dialog is a host
//! dialog too, with the title's buttons: the call completes with the
//! button pressed, or with the button the title named as its cancel action
//! when the dialog is dismissed or cannot open. An error dialog shows the
//! code and the title's text and completes when it is dismissed. Invites,
//! achievements and the player picker have no documented result without
//! the system's UI and stay unimplemented.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::{c_char, c_void};

use super::xasync::{self, AsyncBlock, E_ABORT, E_INVALIDARG};
use super::xgameruntime::{
    Guid, HRESULT, Interface, Methods, Object, S_OK, add_ref, missing, put, query_interface, text,
};

pub const PROVIDER: Guid = Guid::new(
    0xdfcd_4649,
    0x4ff8,
    0x4043,
    [0xba, 0x07, 0x35, 0xd6, 0x07, 0xdf, 0x98, 0xb0],
);
/// As Grand Theft Auto VI (2026) asks for it; the slots it calls (0x38,
/// 0x40, 0x88-0x98) take the public API's arguments in the 2020 order.
pub const IID_2026: Guid = Guid::new(
    0xeaf6_69df,
    0x5542,
    0x4590,
    [0x99, 0xa3, 0x8d, 0xc0, 0x61, 0xf8, 0x37, 0xcc],
);

pub static OBJECT: Object = Object::new("game ui", &VTABLE);
pub const INTERFACES: &[Interface] = &[(PROVIDER, IID_2026, &OBJECT)];

type This = *const Object;

static PROFILE: u8 = 0;
static TEXT: u8 = 0;

extern "system" fn profile_card_async(
    _: This,
    block: *mut AsyncBlock,
    _user: *const c_void,
    xuid: u64,
) -> HRESULT {
    tracing::info!(xuid, "profile card: nothing to show");
    xasync::finished(block, (&raw const PROFILE).cast(), S_OK)
}
extern "system" fn profile_card_result(_: This, block: *mut AsyncBlock) -> HRESULT {
    let none = std::ptr::null_mut();
    xasync::XAsyncGetResult(block, (&raw const PROFILE).cast(), 0, none, none.cast())
}

/// Grand Theft Auto VI treats `E_ABORT` as cancelled and goes on; any
/// other failure as an error.
extern "system" fn text_entry_async(
    _: This,
    block: *mut AsyncBlock,
    title: *const c_char,
    description: *const c_char,
    default: *const c_char,
    _scope: u32,
    max_length: u32,
) -> HRESULT {
    let [title, description, default] =
        [title, description, default].map(|t| text(t).unwrap_or_default());
    tracing::info!(title, description, max_length, "text entry");
    xasync::pending_with(block, (&raw const TEXT).cast(), move || {
        match ask(&title, &description, &default, max_length) {
            Some(typed) => {
                tracing::info!(length = typed.chars().count(), "text entry: done");
                let mut bytes = limit(&typed, max_length).into_bytes();
                bytes.push(0);
                Ok(bytes)
            }
            None => {
                tracing::info!("text entry: cancelled");
                Err(E_ABORT)
            }
        }
    })
}
/// Bytes of the result, its NUL included.
extern "system" fn text_entry_size(_: This, block: *mut AsyncBlock, out: *mut u32) -> HRESULT {
    let mut size = 0;
    let hr = xasync::XAsyncGetResultSize(block, &mut size);
    if hr < 0 {
        return hr;
    }
    put(out, size as u32)
}
extern "system" fn text_entry_result(
    _: This,
    block: *mut AsyncBlock,
    size: u32,
    buffer: *mut c_char,
    used: *mut u32,
) -> HRESULT {
    let mut written = 0;
    let identity = (&raw const TEXT).cast();
    let hr = xasync::XAsyncGetResult(block, identity, size as usize, buffer.cast(), &mut written);
    if hr < 0 || used.is_null() {
        return hr;
    }
    put(used, written as u32)
}

static MESSAGE: u8 = 0;
static ERROR: u8 = 0;
/// `XGameUiMessageDialogButton::Third`, the last of the three buttons.
const THIRD: u32 = 2;

/// `XGameUiShowMessageDialogAsync`: "The index of the button pressed by
/// the user is contained in the result." A button exists for each text
/// the title gives; `cancel` "represents the cancel action", so it is the
/// result when the dialog is dismissed without a button (or cannot open
/// on this host). A default or cancel button that is none of the three
/// is refused.
#[allow(clippy::too_many_arguments)]
extern "system" fn message_dialog_async(
    _: This,
    block: *mut AsyncBlock,
    title: *const c_char,
    content: *const c_char,
    first: *const c_char,
    second: *const c_char,
    third: *const c_char,
    default: u32,
    cancel: u32,
) -> HRESULT {
    if default > THIRD || cancel > THIRD {
        return E_INVALIDARG;
    }
    let [title, content] = [title, content].map(|t| text(t).unwrap_or_default());
    let buttons = [first, second, third].map(text);
    tracing::info!(title, content, "message dialog");
    xasync::pending_with(block, (&raw const MESSAGE).cast(), move || {
        let pressed = choose(&title, &content, &buttons, default);
        tracing::info!(?pressed, cancel, "message dialog: done");
        Ok(pressed.unwrap_or(cancel).to_le_bytes().to_vec())
    })
}
/// `XGameUiShowMessageDialogResult`: the button, once the call is over.
extern "system" fn message_dialog_result(
    _: This,
    block: *mut AsyncBlock,
    out: *mut u32,
) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    let mut button = 0_u32;
    let identity = (&raw const MESSAGE).cast();
    let none = std::ptr::null_mut();
    let hr = xasync::XAsyncGetResult(
        block,
        identity,
        size_of::<u32>(),
        (&raw mut button).cast(),
        none,
    );
    if hr < 0 {
        return hr;
    }
    put(out, button)
}

/// `XGameUiShowErrorDialogAsync`: "The async operation completes when the
/// user dismisses the error dialog." There is no service to look the code
/// up, so the dialog shows the code and the title's own text; on a host
/// without dialogs the error is only logged and the call completes.
extern "system" fn error_dialog_async(
    _: This,
    block: *mut AsyncBlock,
    code: HRESULT,
    context: *const c_char,
) -> HRESULT {
    let content = error_text(code, &text(context).unwrap_or_default());
    tracing::warn!(content, "error dialog");
    xasync::pending_with(block, (&raw const ERROR).cast(), move || {
        choose("Error", &content, &[Some("OK".to_owned()), None, None], 0);
        Ok(Vec::new())
    })
}
extern "system" fn error_dialog_result(_: This, block: *mut AsyncBlock) -> HRESULT {
    let none = std::ptr::null_mut();
    xasync::XAsyncGetResult(block, (&raw const ERROR).cast(), 0, none, none.cast())
}

/// What an error dialog says: the title's text, if any, and the code.
fn error_text(code: HRESULT, context: &str) -> String {
    let code = format!("Error code 0x{:08X}", code as u32);
    if context.is_empty() {
        code
    } else {
        format!("{context}\n\n{code}")
    }
}

/// `XGameUiSetNotificationPositionHint`: "a hint to the operating system
/// on where to display toast notifications" that "the system may choose
/// to ignore". No toast is ever shown here, so it is accepted and unused.
extern "system" fn notification_position(_: This, position: u32) -> HRESULT {
    tracing::debug!(position, "notification position hint (unused)");
    S_OK
}

/// At most `max` characters (0: no limit).
fn limit(typed: &str, max: u32) -> String {
    match max {
        0 => typed.to_owned(),
        max => typed.chars().take(max as usize).collect(),
    }
}

/// Asks the user in a modal host dialog; `None` if dismissed.
#[cfg(all(not(test), windows))]
fn ask(title: &str, description: &str, default: &str, max: u32) -> Option<String> {
    dialog::ask(title, description, default, max)
}

/// Tests answer for the user: "typed" for a title starting with "Type",
/// cancelled otherwise.
#[cfg(test)]
fn ask(title: &str, _: &str, default: &str, _: u32) -> Option<String> {
    title.starts_with("Type").then(|| format!("{default}typed"))
}

/// No native dialog exists yet on other hosts, so text entry is cancelled.
#[cfg(all(not(test), not(windows)))]
fn ask(title: &str, _: &str, _: &str, _: u32) -> Option<String> {
    tracing::warn!(title, "text entry dialog is not available on this host");
    None
}

/// Shows a message with a button for each text given, `default` focused;
/// the index of the button pressed, `None` if dismissed without one.
#[cfg(all(not(test), windows))]
fn choose(title: &str, content: &str, buttons: &[Option<String>; 3], default: u32) -> Option<u32> {
    dialog::message(title, content, buttons, default)
}

/// Tests answer for the user: a title "Press <label>" presses the button
/// with that label; any other dialog is dismissed. What was shown is kept.
#[cfg(test)]
fn choose(title: &str, content: &str, buttons: &[Option<String>; 3], _: u32) -> Option<u32> {
    let mut shown = tests::SHOWN.lock().unwrap_or_else(|e| e.into_inner());
    shown.push(format!("{title}|{content}"));
    let label = title.strip_prefix("Press ")?;
    let pressed = buttons.iter().position(|b| b.as_deref() == Some(label));
    pressed.map(|i| i as u32)
}

/// No native dialog exists yet on other hosts, so a message is dismissed.
#[cfg(all(not(test), not(windows)))]
fn choose(title: &str, content: &str, _: &[Option<String>; 3], _: u32) -> Option<u32> {
    tracing::warn!(
        title,
        content,
        "message dialog is not available on this host"
    );
    None
}

#[cfg(windows)]
mod dialog {
    use windows_sys::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus;
    use windows_sys::Win32::UI::WindowsAndMessaging::{self as wm, DLGTEMPLATE};

    const EDIT: i32 = 100;
    // Edit control messages (the controls headers).
    const EM_SETSEL: u32 = 0xb1;
    const EM_LIMITTEXT: u32 = 0xc5;

    /// The dialog's state, behind its user data while it runs.
    struct Entry {
        max: u32,
        typed: Option<String>,
    }

    /// An in-memory dialog template (dialog units, the system font): a
    /// topmost, centred, captioned popup and its controls.
    struct Template(Vec<u16>);

    /// Window classes of controls, by atom.
    const BUTTON: u16 = 0x80;
    const EDIT_BOX: u16 = 0x81;
    const LABEL: u16 = 0x82;

    impl Template {
        fn dword(&mut self, v: u32) {
            self.0.extend([v as u16, (v >> 16) as u16]);
        }

        fn text(&mut self, s: &str) {
            self.0.extend(s.encode_utf16().filter(|&c| c != 0));
            self.0.push(0);
        }

        /// A dialog `width` by `height` that will hold `items` controls.
        fn new(title: &str, items: u16, width: u16, height: u16) -> Template {
            let mut t = Template(Vec::new());
            let style = wm::WS_POPUP | wm::WS_CAPTION | wm::WS_SYSMENU;
            let flags = (wm::DS_MODALFRAME | wm::DS_CENTER | wm::DS_SETFOREGROUND) as u32;
            t.dword(style | flags);
            t.dword(wm::WS_EX_TOPMOST);
            // Items, x, y, cx, cy, no menu, the default class.
            t.0.extend([items, 0, 0, width, height, 0, 0]);
            t.text(title);
            t
        }

        fn item(&mut self, style: u32, rect: [u16; 4], id: u16, class: u16, caption: &str) {
            if self.0.len() % 2 == 1 {
                self.0.push(0); // items start on a DWORD
            }
            self.dword(wm::WS_CHILD | wm::WS_VISIBLE | style);
            self.dword(0);
            self.0.extend(rect);
            self.0.extend([id, 0xffff, class]);
            self.text(caption);
            self.0.push(0); // no creation data
        }

        /// Runs the dialog modally; what its procedure ended it with, 0 or
        /// less if it could not be shown.
        fn run(
            &self,
            procedure: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> isize,
            parameter: LPARAM,
        ) -> isize {
            // The template must start on a DWORD.
            let template: Vec<u32> = self
                .0
                .chunks(2)
                .map(|c| u32::from(c[0]) | u32::from(c.get(1).copied().unwrap_or(0)) << 16)
                .collect();
            // SAFETY: a complete template and one of our dialog procedures;
            // the caller keeps what `parameter` points at alive meanwhile.
            unsafe {
                wm::DialogBoxIndirectParamW(
                    GetModuleHandleW(std::ptr::null()),
                    template.as_ptr().cast::<DLGTEMPLATE>(),
                    std::ptr::null_mut(),
                    Some(procedure),
                    parameter,
                )
            }
        }
    }

    /// The description, an edit box with the default text, OK and Cancel;
    /// Enter accepts, Esc cancels.
    #[allow(dead_code)] // unused in test builds
    pub(super) fn ask(title: &str, description: &str, default: &str, max: u32) -> Option<String> {
        let mut t = Template::new(title, 4, 240, 62);
        let tab = wm::WS_TABSTOP;
        t.item(0, [7, 7, 226, 10], 0xffff, LABEL, description);
        let edit_style = wm::WS_BORDER | tab | wm::ES_AUTOHSCROLL as u32;
        t.item(edit_style, [7, 20, 226, 12], EDIT as u16, EDIT_BOX, default);
        let ok = wm::BS_DEFPUSHBUTTON as u32 | tab;
        t.item(ok, [129, 40, 50, 14], wm::IDOK as u16, BUTTON, "OK");
        let cancel = wm::BS_PUSHBUTTON as u32 | tab;
        t.item(
            cancel,
            [183, 40, 50, 14],
            wm::IDCANCEL as u16,
            BUTTON,
            "Cancel",
        );
        let mut entry = Entry { max, typed: None };
        // `entry` outlives the modal dialog.
        t.run(procedure, (&raw mut entry) as LPARAM);
        entry.typed
    }

    /// Control id of a message dialog's first button; the others follow.
    pub(super) const FIRST_BUTTON: i32 = 200;

    /// The text and a button for each label given, right-aligned in their
    /// order; `default` has the focus and takes Enter. The index of the
    /// button pressed; `None` for Esc, the close box, or no dialog.
    #[allow(dead_code)] // unused in test builds
    pub(super) fn message(
        title: &str,
        content: &str,
        buttons: &[Option<String>; 3],
        default: u32,
    ) -> Option<u32> {
        // Room for the text: about 55 characters a line, at most 12 lines.
        let lines: usize = content.lines().map(|l| l.chars().count() / 55 + 1).sum();
        let text_height = 8 * lines.clamp(1, 12) as u16 + 2;
        let shown: Vec<(usize, &str)> = buttons
            .iter()
            .enumerate()
            .filter_map(|(i, b)| Some((i, b.as_deref()?)))
            .collect();
        let mut t = Template::new(title, 1 + shown.len() as u16, 240, text_height + 36);
        t.item(0, [7, 7, 226, text_height], 0xffff, LABEL, content);
        let mut x = 240 - 7 - 74 * shown.len() as u16 + 4;
        for &(i, label) in &shown {
            let kind = if i as u32 == default {
                wm::BS_DEFPUSHBUTTON
            } else {
                wm::BS_PUSHBUTTON
            };
            let rect = [x, text_height + 14, 70, 14];
            let id = (FIRST_BUTTON + i as i32) as u16;
            t.item(kind as u32 | wm::WS_TABSTOP, rect, id, BUTTON, label);
            x += 74;
        }
        match t.run(message_procedure, default as LPARAM) {
            // The procedure ends the dialog with the button's index + 1.
            pressed @ 1..=3 => Some(pressed as u32 - 1),
            _ => None,
        }
    }

    extern "system" fn message_procedure(
        dialog: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> isize {
        // SAFETY: our dialog; plain window calls on it and its controls.
        unsafe {
            match message {
                wm::WM_INITDIALOG => {
                    // `lparam` is the default button's index.
                    let button = wm::GetDlgItem(dialog, FIRST_BUTTON + lparam as i32);
                    if button.is_null() {
                        return 1; // none such: the system picks the focus
                    }
                    SetFocus(button);
                    0 // focus set here
                }
                wm::WM_COMMAND => match (wparam & 0xffff) as i32 {
                    // Only a button the dialog has can be pressed.
                    id @ FIRST_BUTTON..=202 if !wm::GetDlgItem(dialog, id).is_null() => {
                        wm::EndDialog(dialog, (id - FIRST_BUTTON + 1) as isize);
                        1
                    }
                    wm::IDCANCEL => {
                        wm::EndDialog(dialog, 0);
                        1
                    }
                    _ => 0,
                },
                _ => 0,
            }
        }
    }

    extern "system" fn procedure(
        dialog: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> isize {
        // SAFETY: our dialog; its user data is the `Entry` from `ask`,
        // stored at WM_INITDIALOG.
        unsafe {
            let edit = wm::GetDlgItem(dialog, EDIT);
            match message {
                wm::WM_INITDIALOG => {
                    wm::SetWindowLongPtrW(dialog, wm::GWLP_USERDATA, lparam);
                    let entry = &*(lparam as *const Entry);
                    wm::SendMessageW(edit, EM_LIMITTEXT, entry.max as WPARAM, 0);
                    wm::SendMessageW(edit, EM_SETSEL, 0, -1);
                    SetFocus(edit);
                    0 // focus set here
                }
                wm::WM_COMMAND => {
                    let entry =
                        &mut *(wm::GetWindowLongPtrW(dialog, wm::GWLP_USERDATA) as *mut Entry);
                    match (wparam & 0xffff) as i32 {
                        wm::IDOK => {
                            let mut buffer =
                                vec![0u16; wm::GetWindowTextLengthW(edit) as usize + 1];
                            let n =
                                wm::GetWindowTextW(edit, buffer.as_mut_ptr(), buffer.len() as i32);
                            entry.typed =
                                Some(String::from_utf16_lossy(&buffer[..n.max(0) as usize]));
                            wm::EndDialog(dialog, 1);
                        }
                        wm::IDCANCEL => {
                            wm::EndDialog(dialog, 0);
                        }
                        _ => return 0,
                    }
                    1
                }
                _ => 0,
            }
        }
    }
}

// Sending an invite, the achievements and the player picker show system
// UI over services this runtime does not have. The public reference does
// not say what their calls complete with when that UI shows nothing or is
// cancelled, so their slots keep failing with `E_NOTIMPL` and begin no
// call (the test `invites_achievements_and_the_picker_fail_untouched`).
static VTABLE: Methods<20> = Methods([
    query_interface as *const (),
    add_ref as *const (),
    add_ref as *const (),
    message_dialog_async as *const (),  // 0x18
    message_dialog_result as *const (), // 0x20
    missing::<5> as *const (),          // 0x28 send invite
    missing::<6> as *const (),          // 0x30
    profile_card_async as *const (),    // 0x38
    profile_card_result as *const (),   // 0x40
    missing::<9> as *const (),          // 0x48 achievements
    missing::<10> as *const (),         // 0x50
    missing::<11> as *const (),         // 0x58 player picker
    missing::<12> as *const (),         // 0x60
    missing::<13> as *const (),         // 0x68
    error_dialog_async as *const (),    // 0x70
    error_dialog_result as *const (),   // 0x78
    notification_position as *const (), // 0x80
    text_entry_async as *const (),      // 0x88
    text_entry_size as *const (),       // 0x90
    text_entry_result as *const (),     // 0x98
]);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::xgameruntime::method;

    /// Message dialogs the test `choose` was asked to show: "title|text".
    pub(super) static SHOWN: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

    fn block() -> AsyncBlock {
        AsyncBlock {
            queue: std::ptr::null(),
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        }
    }

    #[test]
    fn dialogs_end_with_what_was_typed() {
        let this: This = &OBJECT;
        let mut b = block();
        type Card = extern "system" fn(This, *mut AsyncBlock, *const c_void, u64) -> HRESULT;
        type Done = extern "system" fn(This, *mut AsyncBlock) -> HRESULT;
        assert_eq!(
            method::<Card>(&OBJECT, 0x38)(this, &mut b, std::ptr::null(), 1),
            S_OK
        );
        assert_eq!(xasync::XAsyncGetStatus(&mut b, true), S_OK);
        assert_eq!(method::<Done>(&OBJECT, 0x40)(this, &mut b), S_OK);

        type Entry = extern "system" fn(
            This,
            *mut AsyncBlock,
            *const c_char,
            *const c_char,
            *const c_char,
            u32,
            u32,
        ) -> HRESULT;
        type Size = extern "system" fn(This, *mut AsyncBlock, *mut u32) -> HRESULT;
        type Result_ =
            extern "system" fn(This, *mut AsyncBlock, u32, *mut c_char, *mut u32) -> HRESULT;
        let null = std::ptr::null();
        let entry = method::<Entry>(&OBJECT, 0x88);
        // Dismissed: cancelled.
        let mut b = block();
        assert_eq!(
            entry(this, &mut b, c"Name".as_ptr(), null, null, 0, 16),
            S_OK
        );
        assert_eq!(xasync::XAsyncGetStatus(&mut b, true), E_ABORT);
        let mut size = 0;
        assert_eq!(
            method::<Size>(&OBJECT, 0x90)(this, &mut b, &mut size),
            E_ABORT
        );
        // Typed: the text, cut to the limit, with its NUL.
        let mut b = block();
        let default = c"abc".as_ptr();
        assert_eq!(
            entry(this, &mut b, c"Type".as_ptr(), null, default, 0, 6),
            S_OK
        );
        assert_eq!(xasync::XAsyncGetStatus(&mut b, true), S_OK);
        assert_eq!(method::<Size>(&OBJECT, 0x90)(this, &mut b, &mut size), S_OK);
        assert_eq!(size, 7);
        let mut out = [0 as c_char; 7];
        let mut used = 0;
        let result = method::<Result_>(&OBJECT, 0x98);
        assert_eq!(result(this, &mut b, 7, out.as_mut_ptr(), &mut used), S_OK);
        assert_eq!((text(out.as_ptr()).as_deref(), used), (Some("abctyp"), 7));
        assert_eq!(limit("\u{e9}t\u{e9}s", 3), "\u{e9}t\u{e9}");
    }

    type Message = extern "system" fn(
        This,
        *mut AsyncBlock,
        *const c_char,
        *const c_char,
        *const c_char,
        *const c_char,
        *const c_char,
        u32,
        u32,
    ) -> HRESULT;
    type Button = extern "system" fn(This, *mut AsyncBlock, *mut u32) -> HRESULT;

    /// Shows a message dialog with buttons "One" and "Three" (no second
    /// one); the status it completes with and the button in its result.
    fn message(title: &std::ffi::CStr, default: u32, cancel: u32) -> (HRESULT, HRESULT, u32) {
        let this: This = &OBJECT;
        let mut b = block();
        let hr = method::<Message>(&OBJECT, 0x18)(
            this,
            &mut b,
            title.as_ptr(),
            c"Text".as_ptr(),
            c"One".as_ptr(),
            std::ptr::null(),
            c"Three".as_ptr(),
            default,
            cancel,
        );
        if hr < 0 {
            assert_eq!(b.internal, [0; 4], "no call was begun");
            return (hr, hr, u32::MAX);
        }
        let status = xasync::XAsyncGetStatus(&mut b, true);
        let mut button = u32::MAX;
        let result = method::<Button>(&OBJECT, 0x20)(this, &mut b, &mut button);
        (status, result, button)
    }

    #[test]
    fn message_dialogs_end_with_the_button_pressed_or_the_cancel_button() {
        // Pressed: the button's index, whatever default and cancel are.
        assert_eq!(message(c"Press One", 2, 2), (S_OK, S_OK, 0));
        assert_eq!(message(c"Press Three", 0, 0), (S_OK, S_OK, 2));
        // Dismissed: the button the title named as its cancel action.
        for cancel in 0..=2 {
            assert_eq!(message(c"Dismiss", 0, cancel), (S_OK, S_OK, cancel));
        }
        // A button with no text does not exist, so it cannot be pressed.
        assert_eq!(message(c"Press Two", 0, 2), (S_OK, S_OK, 2));
        // Neither default nor cancel can be a fourth button.
        let invalid = (E_INVALIDARG, E_INVALIDARG, u32::MAX);
        assert_eq!(message(c"Press One", 3, 0), invalid);
        assert_eq!(message(c"Press One", 0, 3), invalid);
        assert_eq!(E_INVALIDARG as u32, 0x8007_0057);

        // The result needs somewhere to go, and is another call's to take
        // only once.
        let this: This = &OBJECT;
        let mut b = block();
        let null = std::ptr::null();
        let show = method::<Message>(&OBJECT, 0x18);
        assert_eq!(
            show(
                this,
                &mut b,
                c"Press A".as_ptr(),
                null,
                c"A".as_ptr(),
                null,
                null,
                0,
                1
            ),
            S_OK
        );
        assert_eq!(xasync::XAsyncGetStatus(&mut b, true), S_OK);
        let result = method::<Button>(&OBJECT, 0x20);
        assert_eq!(result(this, &mut b, std::ptr::null_mut()), E_INVALIDARG);
        let mut button = u32::MAX;
        assert_eq!(result(this, &mut b, &mut button), S_OK);
        assert_eq!(button, 0);
    }

    #[test]
    fn error_dialogs_complete_when_dismissed_and_the_position_hint_is_accepted() {
        type Error = extern "system" fn(This, *mut AsyncBlock, HRESULT, *const c_char) -> HRESULT;
        type Done = extern "system" fn(This, *mut AsyncBlock) -> HRESULT;
        let this: This = &OBJECT;
        let show = method::<Error>(&OBJECT, 0x70);
        let done = method::<Done>(&OBJECT, 0x78);
        let code = 0x8923_5200_u32 as i32;
        for context in [c"save 7c1f failed".as_ptr(), std::ptr::null()] {
            let mut b = block();
            assert_eq!(show(this, &mut b, code, context), S_OK);
            assert_eq!(xasync::XAsyncGetStatus(&mut b, true), S_OK);
            assert_eq!(done(this, &mut b), S_OK);
        }
        // The dialog showed the title's text and the code.
        let shown = SHOWN.lock().unwrap().clone();
        let expected = "Error|save 7c1f failed\n\nError code 0x89235200".to_owned();
        assert!(shown.contains(&expected), "{shown:?}");
        assert_eq!(error_text(code, ""), "Error code 0x89235200");

        // XGameUiSetNotificationPositionHint: any of the six positions.
        type Hint = extern "system" fn(This, u32) -> HRESULT;
        for position in 0..6 {
            assert_eq!(method::<Hint>(&OBJECT, 0x80)(this, position), S_OK);
        }
    }

    /// Slots with no documented offline result, called with the public
    /// API's arguments: `E_NOTIMPL`, no call begun, nothing written.
    #[test]
    fn invites_achievements_and_the_picker_fail_untouched() {
        use crate::modules::xasync::E_NOTIMPL;
        let this: This = &OBJECT;
        let user = std::ptr::null::<c_void>();
        let null = std::ptr::null::<c_char>();
        assert_eq!(E_NOTIMPL as u32, 0x8000_4001);
        type Done = extern "system" fn(This, *mut AsyncBlock) -> HRESULT;

        // XGameUiShowSendGameInviteAsync(async, requestingUser,
        // sessionConfigurationId, sessionTemplateName, sessionId,
        // invitationText, customActivationContext) and its result.
        type Invite = extern "system" fn(
            This,
            *mut AsyncBlock,
            *const c_void,
            *const c_char,
            *const c_char,
            *const c_char,
            *const c_char,
            *const c_char,
        ) -> HRESULT;
        let mut b = block();
        let invite = method::<Invite>(&OBJECT, 0x28);
        assert_eq!(
            invite(
                this,
                &mut b,
                user,
                c"scid".as_ptr(),
                c"t".as_ptr(),
                c"s".as_ptr(),
                null,
                null
            ),
            E_NOTIMPL
        );
        assert_eq!(b.internal, [0; 4]);
        assert_eq!(method::<Done>(&OBJECT, 0x30)(this, &mut b), E_NOTIMPL);

        // XGameUiShowAchievementsAsync(async, requestingUser, titleId).
        type Achievements =
            extern "system" fn(This, *mut AsyncBlock, *const c_void, u32) -> HRESULT;
        let mut b = block();
        assert_eq!(
            method::<Achievements>(&OBJECT, 0x48)(this, &mut b, user, 1),
            E_NOTIMPL
        );
        assert_eq!(b.internal, [0; 4]);
        assert_eq!(method::<Done>(&OBJECT, 0x50)(this, &mut b), E_NOTIMPL);

        // XGameUiShowPlayerPickerAsync(async, requestingUser, promptText,
        // selectFromPlayersCount, selectFromPlayers,
        // preSelectedPlayersCount, preSelectedPlayers, minSelectionCount,
        // maxSelectionCount), its result count and its result.
        type Picker = extern "system" fn(
            This,
            *mut AsyncBlock,
            *const c_void,
            *const c_char,
            u32,
            *const u64,
            u32,
            *const u64,
            u32,
            u32,
        ) -> HRESULT;
        type Count = extern "system" fn(This, *mut AsyncBlock, *mut u32) -> HRESULT;
        type Picked = extern "system" fn(This, *mut AsyncBlock, u32, *mut u64, *mut u32) -> HRESULT;
        let players = [7_u64, 8];
        let mut b = block();
        let pick = method::<Picker>(&OBJECT, 0x58);
        assert_eq!(
            pick(
                this,
                &mut b,
                user,
                c"Pick".as_ptr(),
                2,
                players.as_ptr(),
                0,
                std::ptr::null(),
                1,
                2
            ),
            E_NOTIMPL
        );
        assert_eq!(b.internal, [0; 4]);
        let mut count = 0x55;
        assert_eq!(
            method::<Count>(&OBJECT, 0x60)(this, &mut b, &mut count),
            E_NOTIMPL
        );
        let (mut picked, mut used) = ([0xa5a5_u64; 2], 0x55);
        assert_eq!(
            method::<Picked>(&OBJECT, 0x68)(this, &mut b, 2, picked.as_mut_ptr(), &mut used),
            E_NOTIMPL
        );
        assert_eq!((count, picked, used), (0x55, [0xa5a5; 2], 0x55));
    }

    /// The real host dialogs, driven the way a click drives them: the
    /// command of a control is posted to the dialog once it is up.
    #[cfg(windows)]
    #[test]
    fn host_dialogs_end_with_the_command_they_are_sent() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            FindWindowW, IDCANCEL, IDOK, PostMessageW, WM_COMMAND,
        };

        /// Posts `command` to the dialog titled `title` when it appears;
        /// whether it did within ten seconds.
        fn press(title: &str, command: i32) -> std::thread::JoinHandle<bool> {
            let title: Vec<u16> = title.encode_utf16().chain([0]).collect();
            std::thread::spawn(move || {
                for _ in 0..1000 {
                    // SAFETY: a NUL-terminated title; posting to a window
                    // that has gone is harmless.
                    unsafe {
                        let window = FindWindowW(std::ptr::null(), title.as_ptr());
                        if !window.is_null() {
                            return PostMessageW(window, WM_COMMAND, command as usize, 0) != 0;
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                false
            })
        }

        let id = std::process::id();
        let buttons = [Some("Yes".to_owned()), None, Some("No".to_owned())];
        let cases = [
            (dialog::FIRST_BUTTON, Some(0)),
            (dialog::FIRST_BUTTON + 2, Some(2)),
            // Esc and the close box send the cancel command.
            (IDCANCEL, None),
        ];
        for (n, (command, expected)) in cases.into_iter().enumerate() {
            let title = format!("victory message test {id} {n}");
            let presser = press(&title, command);
            let pressed = dialog::message(&title, "Two lines\nof text", &buttons, 2);
            assert!(presser.join().unwrap(), "the dialog came up: {title}");
            assert_eq!(pressed, expected, "command {command}");
        }
        // A command of a button that does not exist ends nothing; the
        // cancel that follows does.
        let title = format!("victory message test {id} absent");
        let absent = press(&title, dialog::FIRST_BUTTON + 1);
        let pressed = std::thread::scope(|s| {
            let shown = s.spawn(|| dialog::message(&title, "Text", &buttons, 0));
            assert!(absent.join().unwrap());
            std::thread::sleep(std::time::Duration::from_millis(200));
            assert!(!shown.is_finished(), "still open");
            assert!(press(&title, IDCANCEL).join().unwrap());
            shown.join().unwrap()
        });
        assert_eq!(pressed, None);

        // Text entry, over the same template code: OK takes the text.
        let title = format!("victory entry test {id}");
        let presser = press(&title, IDOK);
        let typed = dialog::ask(&title, "Description", "default", 16);
        assert!(presser.join().unwrap());
        assert_eq!(typed.as_deref(), Some("default"));
        let title = format!("victory entry test {id} cancel");
        let presser = press(&title, IDCANCEL);
        assert_eq!(dialog::ask(&title, "Description", "default", 16), None);
        assert!(presser.join().unwrap());
    }
}
