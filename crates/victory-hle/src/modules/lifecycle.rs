//! The title's life cycle. A console title is never told it is closing:
//! quitting it or launching a new game first suspends it (its suspend
//! routines run, and it has a second to save) and then ends the process.
//! Closing the title's window does the same here; a new launch then starts
//! `victory` again, which waits for this process to be gone.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::too_many_arguments)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use windows_sys::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
use windows_sys::Win32::UI::WindowsAndMessaging::{self as wm, HMENU};
use windows_sys::core::{PCSTR, PCWSTR, w};

use crate::{HleModule, hle_export};

type Routine = extern "system" fn(quiesced: u8, context: *mut c_void);

/// Registration id, routine and context of each suspend routine.
static ROUTINES: Mutex<Vec<(usize, Routine, usize)>> = Mutex::new(Vec::new());

fn routines() -> MutexGuard<'static, Vec<(usize, Routine, usize)>> {
    ROUTINES.lock().unwrap_or_else(|e| e.into_inner())
}

/// How long the console lets suspend routines run before it ends the title.
const SUSPEND_DEADLINE: Duration = Duration::from_secs(1);

const ERROR_INVALID_PARAMETER: u32 = 87;

pub extern "system" fn RegisterAppStateChangeNotification(
    routine: Option<Routine>,
    context: *mut c_void,
    registration: *mut usize,
) -> u32 {
    let Some(routine) = routine else {
        return ERROR_INVALID_PARAMETER;
    };
    if registration.is_null() {
        return ERROR_INVALID_PARAMETER;
    }
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    routines().push((id, routine, context as usize));
    // SAFETY: checked non-null; the title passes room for its registration.
    unsafe { registration.write(id) };
    0
}

pub extern "system" fn UnregisterAppStateChangeNotification(registration: usize) {
    routines().retain(|&(id, ..)| id != registration);
}

/// The window procedure a title window had before ours.
const TITLE_PROC: PCWSTR = w!("victory.wndproc");

pub extern "system" fn CreateWindowExW(
    ex_style: u32,
    class: PCWSTR,
    name: PCWSTR,
    style: u32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    parent: HWND,
    menu: HMENU,
    instance: HINSTANCE,
    param: *const c_void,
) -> HWND {
    let (x, y, width, height) = fit(parent, x, y, width, height);
    // SAFETY: the title's own arguments, forwarded (the size fitted).
    let hwnd = unsafe {
        wm::CreateWindowExW(
            ex_style, class, name, style, x, y, width, height, parent, menu, instance, param,
        )
    };
    hook(hwnd);
    hwnd
}

/// See [`CreateWindowExW`].
pub extern "system" fn CreateWindowExA(
    ex_style: u32,
    class: PCSTR,
    name: PCSTR,
    style: u32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    parent: HWND,
    menu: HMENU,
    instance: HINSTANCE,
    param: *const c_void,
) -> HWND {
    let (x, y, width, height) = fit(parent, x, y, width, height);
    // SAFETY: the title's own arguments, forwarded (the size fitted).
    let hwnd = unsafe {
        wm::CreateWindowExA(
            ex_style, class, name, style, x, y, width, height, parent, menu, instance, param,
        )
    };
    hook(hwnd);
    hwnd
}

/// A console title sizes its window to its render target (Grand Theft
/// Auto VI: 3840x2160 on a Series X), which can exceed the host's screen.
/// A top-level window larger than the work area is shrunk to fit, keeping
/// its proportions, and centred; what it shows is scaled to it (the swap
/// chain stretches).
fn fit(parent: HWND, x: i32, y: i32, width: i32, height: i32) -> (i32, i32, i32, i32) {
    let mut area = RECT::default();
    // SAFETY: `area` receives the primary monitor's work area.
    let known =
        unsafe { wm::SystemParametersInfoW(wm::SPI_GETWORKAREA, 0, (&raw mut area).cast(), 0) };
    let fitted = fit_into(&area, width, height);
    if !parent.is_null() || known == 0 || fitted == (width, height) {
        return (x, y, width, height);
    }
    tracing::info!(width, height, to = ?fitted, "title window fitted to the screen");
    let (w, h) = fitted;
    let left = area.left + (area.right - area.left - w) / 2;
    let top = area.top + (area.bottom - area.top - h) / 2;
    (left, top, w, h)
}

/// `width` x `height` scaled down to fit `area`, or unchanged if it fits
/// (or is `CW_USEDEFAULT`).
fn fit_into(area: &RECT, width: i32, height: i32) -> (i32, i32) {
    let (aw, ah) = (area.right - area.left, area.bottom - area.top);
    if width <= 0 || height <= 0 || aw <= 0 || ah <= 0 || (width <= aw && height <= ah) {
        return (width, height);
    }
    let scale = f64::min(
        f64::from(aw) / f64::from(width),
        f64::from(ah) / f64::from(height),
    );
    (
        (f64::from(width) * scale) as i32,
        (f64::from(height) * scale) as i32,
    )
}

/// Puts our procedure in front of the title's.
fn hook(hwnd: HWND) {
    if hwnd.is_null() {
        return;
    }
    // SAFETY: a window this thread just created. The title's procedure is
    // stored before ours can run.
    unsafe {
        let title = wm::GetWindowLongPtrW(hwnd, wm::GWLP_WNDPROC);
        wm::SetPropW(hwnd, TITLE_PROC, title as *mut c_void);
        wm::SetWindowLongPtrW(hwnd, wm::GWLP_WNDPROC, window_proc as *const () as isize);
    }
}

extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // A user close (close button, Alt+F4, window or taskbar menu); a
    // console title never sees one, nor a WM_CLOSE from it.
    if message == wm::WM_SYSCOMMAND && wparam & 0xFFF0 == wm::SC_CLOSE as usize {
        end_title("window closed", Box::new(|| {}));
        return 0;
    }
    // Alt or F10 puts a window into menu mode: the system runs a modal menu
    // loop on the thread that owns the window, which is the title's. The
    // title then stops (no frames, no input) until a key ends the loop, and
    // nothing shows it, since the window has no menu bar; Grand Theft Auto
    // VI aborts itself about half a minute later. A console has no window
    // menu. The mouse still opens it from the title bar.
    if message == wm::WM_SYSCOMMAND && wparam & 0xFFF0 == wm::SC_KEYMENU as usize {
        return 0;
    }
    // SAFETY: the procedure `hook` stored, called the way the system would.
    unsafe {
        let title = wm::GetPropW(hwnd, TITLE_PROC);
        if message == wm::WM_NCDESTROY {
            wm::RemovePropW(hwnd, TITLE_PROC);
        }
        let title = std::mem::transmute::<*mut c_void, wm::WNDPROC>(title);
        wm::CallWindowProcW(title, hwnd, message, wparam, lparam)
    }
}

/// Suspends the title as the console does when it ends it, runs `then`
/// (once), then ends the process. Returns at once; later calls do nothing.
pub(crate) fn end_title(why: &'static str, then: Box<dyn FnOnce() + Send>) {
    static ENDING: AtomicBool = AtomicBool::new(false);
    if ENDING.swap(true, Ordering::SeqCst) {
        return;
    }
    let suspend = routines().clone();
    tracing::info!(routines = suspend.len(), "{why}: suspending the title");
    // Whoever comes first (the routines or the deadline) runs `then`; the
    // other waits for it, so the process never ends halfway through.
    let then = std::sync::Arc::new(Mutex::new(Some(then)));
    let finish = move |how: &str| {
        let mut then = then.lock().unwrap_or_else(|e| e.into_inner());
        tracing::info!("{how}; ending the title");
        if let Some(then) = then.take() {
            then();
        }
        end();
    };
    let late = finish.clone();
    // Not on the calling thread: titles hand the suspend to their window
    // thread and wait for it inside the routine.
    std::thread::spawn(move || {
        for (_, routine, context) in suspend {
            routine(1, context as *mut c_void);
        }
        finish("title suspended");
    });
    std::thread::spawn(move || {
        std::thread::sleep(SUSPEND_DEADLINE);
        late("title did not finish suspending in time");
    });
}

/// Set in the environment of a relaunched `victory`: an inherited handle
/// of the process it replaces.
const RELAUNCHED_FROM: &str = "VICTORY_RELAUNCHED_FROM";

/// Starts `victory` again with `args` (after the title has suspended); the
/// new process waits until this one has ended.
pub(crate) fn relaunch(args: Vec<std::ffi::OsString>) {
    relaunch_with(args, |mut command| command.spawn().map(|child| child.id()));
}

/// [`relaunch`], with `spawn` starting the process it has described and
/// returning its id.
fn relaunch_with(
    args: Vec<std::ffi::OsString>,
    spawn: impl FnOnce(std::process::Command) -> std::io::Result<u32>,
) {
    use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE};
    let mut me: HANDLE = std::ptr::null_mut();
    // SAFETY: an inheritable duplicate of our own process handle.
    let duplicated = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            GetCurrentProcess(),
            GetCurrentProcess(),
            &mut me,
            0,
            1,
            DUPLICATE_SAME_ACCESS,
        )
    } != 0;
    let spawned = std::env::current_exe().and_then(|exe| {
        let mut command = std::process::Command::new(exe);
        command.args(&args);
        if duplicated {
            command.env(RELAUNCHED_FROM, (me as usize).to_string());
        }
        spawn(command)
    });
    match spawned {
        Ok(pid) => tracing::info!(pid, ?args, "relaunched"),
        Err(e) => tracing::warn!(%e, "relaunch failed; the title just ends"),
    }
}

/// In a relaunched `victory`: waits (30 s at most) for the process it
/// replaces to be gone, so the two never share the title's files.
pub fn wait_for_previous() {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Threading::WaitForSingleObject;
    let Some(handle) = std::env::var(RELAUNCHED_FROM)
        .ok()
        .and_then(|h| h.parse::<usize>().ok())
    else {
        return;
    };
    // SAFETY: removing our own variable before any other thread reads it.
    unsafe { std::env::remove_var(RELAUNCHED_FROM) };
    let handle = handle as HANDLE;
    // SAFETY: the inherited process handle the previous process gave us.
    unsafe {
        WaitForSingleObject(handle, 30_000);
        CloseHandle(handle);
    }
}

fn end() {
    // Compiled host pipelines kept for the next run.
    #[cfg(windows)]
    victory_gpu::pipeline::save_pipeline_cache();
    // SAFETY: ends this process, as the console ends a suspended title.
    unsafe { TerminateProcess(GetCurrentProcess(), 0) };
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "lifecycle",
        dlls: &[
            "api-ms-win-core-psm-appnotify-*",
            "ext-ms-win-rtcore-ntuser-window-*",
        ],
        exports: vec![
            hle_export!(RegisterAppStateChangeNotification),
            hle_export!(UnregisterAppStateChangeNotification),
            hle_export!(CreateWindowExW),
            hle_export!(CreateWindowExA),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern "system" fn noop(_: u8, _: *mut c_void) {}

    #[test]
    fn registrations_come_and_go() {
        let (mut a, mut b) = (0, 0);
        assert_eq!(
            RegisterAppStateChangeNotification(Some(noop), std::ptr::null_mut(), &mut a),
            0
        );
        assert_eq!(
            RegisterAppStateChangeNotification(Some(noop), std::ptr::null_mut(), &mut b),
            0
        );
        UnregisterAppStateChangeNotification(a);
        let left: Vec<_> = routines().iter().map(|r| r.0).collect();
        assert_eq!(left, [b]);
    }

    #[test]
    fn oversized_windows_shrink_to_the_screen() {
        let area = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        assert_eq!(fit_into(&area, 3840, 2160), (1848, 1040));
        assert_eq!(fit_into(&area, 1280, 720), (1280, 720));
        assert_eq!(
            fit_into(&area, wm::CW_USEDEFAULT, 0),
            (wm::CW_USEDEFAULT, 0)
        );
    }

    #[test]
    fn hooked_windows_still_reach_the_title_procedure() {
        let null = std::ptr::null_mut();
        let hwnd = CreateWindowExW(
            0,
            w!("STATIC"),
            w!("before"),
            0,
            0,
            0,
            10,
            10,
            null,
            null,
            null,
            std::ptr::null(),
        );
        assert!(!hwnd.is_null());
        let mut text = [0u16; 16];
        // SAFETY: our window; the buffer holds 16 characters.
        unsafe {
            wm::SetWindowTextW(hwnd, w!("after"));
            let n = wm::GetWindowTextW(hwnd, text.as_mut_ptr(), 16);
            assert_eq!(String::from_utf16_lossy(&text[..n as usize]), "after");
            assert!(!wm::GetPropW(hwnd, TITLE_PROC).is_null());
            wm::DestroyWindow(hwnd);
        }
    }

    thread_local! {
        /// The system commands `recording` received, low four bits cleared.
        static COMMANDS: std::cell::RefCell<Vec<usize>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    /// Stands in for a title's procedure: notes system commands and acts
    /// on none of them (no menu loop, no minimising).
    extern "system" fn recording(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == wm::WM_SYSCOMMAND {
            COMMANDS.with(|c| c.borrow_mut().push(wparam & 0xFFF0));
            return 0;
        }
        // SAFETY: the system's default procedure, called as the system would.
        unsafe { wm::DefWindowProcW(hwnd, message, wparam, lparam) }
    }

    #[test]
    fn the_keyboard_menu_never_reaches_the_title() {
        let null = std::ptr::null_mut();
        let hwnd = CreateWindowExW(
            0,
            w!("STATIC"),
            w!("menu"),
            0,
            0,
            0,
            10,
            10,
            null,
            null,
            null,
            std::ptr::null(),
        );
        assert!(!hwnd.is_null());
        // SAFETY: our window, owned by this thread, so each message is
        // handled before SendMessageW returns. `recording` stands in for the
        // title's procedure and the original is put back before the window
        // is destroyed.
        unsafe {
            let title = wm::GetPropW(hwnd, TITLE_PROC);
            wm::SetPropW(hwnd, TITLE_PROC, recording as *const () as *mut c_void);
            let command = |c: u32, key: u8| {
                wm::SendMessageW(hwnd, wm::WM_SYSCOMMAND, c as usize, LPARAM::from(key))
            };
            command(wm::SC_KEYMENU, 0); // Alt or F10
            command(wm::SC_KEYMENU, b' '); // Alt+Space
            command(wm::SC_KEYMENU | 1, 0); // the low bits are the system's
            command(wm::SC_MINIMIZE, 0);
            wm::SetPropW(hwnd, TITLE_PROC, title);
            wm::DestroyWindow(hwnd);
        }
        let seen = COMMANDS.with(|c| c.borrow().clone());
        assert_eq!(seen, [wm::SC_MINIMIZE as usize]);
    }

    /// Which part the child process plays; unset in ordinary runs.
    const PART: &str = "VICTORY_LIFECYCLE_CHILD";

    /// A suspend routine that takes `context` milliseconds, then says it
    /// is done.
    extern "system" fn suspending(quiesced: u8, context: *mut c_void) {
        std::thread::sleep(Duration::from_millis(context as u64));
        println!("SUSPENDED quiesced={quiesced}");
    }

    /// In a child process of the tests below; does nothing otherwise.
    /// `close` and `slow-close`: a title with one suspend routine (0.3 s,
    /// or 3 s) whose window the user closes. `previous`: a process that
    /// lives 2.5 s. `relaunched`: a new `victory` that waits for the one
    /// it replaces.
    #[test]
    fn lifecycle_child() {
        let Ok(part) = std::env::var(PART) else {
            return;
        };
        crate::testing::print_logs();
        match part.as_str() {
            "close" | "slow-close" => {
                let takes: usize = if part == "close" { 300 } else { 3000 };
                let mut id = 0;
                let registered = RegisterAppStateChangeNotification(
                    Some(suspending),
                    takes as *mut c_void,
                    &mut id,
                );
                let null = std::ptr::null_mut();
                let hwnd = CreateWindowExW(
                    0,
                    w!("STATIC"),
                    w!("title"),
                    0,
                    0,
                    0,
                    10,
                    10,
                    null,
                    null,
                    null,
                    std::ptr::null(),
                );
                println!("CLOSING registered={registered} window={}", !hwnd.is_null());
                // SAFETY: our window, owned by this thread.
                let answer =
                    unsafe { wm::SendMessageW(hwnd, wm::WM_SYSCOMMAND, wm::SC_CLOSE as usize, 0) };
                // The close is taken at once; the title goes on meanwhile.
                println!("CLOSE RETURNED {answer}");
                std::thread::sleep(Duration::from_secs(10));
                println!("STILL RUNNING");
            }
            "previous" => std::thread::sleep(Duration::from_millis(2500)),
            "relaunched" => {
                wait_for_previous();
                println!(
                    "WAITED handle_left={}",
                    std::env::var(RELAUNCHED_FROM).is_ok()
                );
            }
            _ => {}
        }
    }

    fn child(part: &str) -> (std::process::ExitStatus, String, String) {
        let test = "modules::lifecycle::tests::lifecycle_child";
        let out = crate::testing::child(test, &[(PART, part.as_ref())]);
        let (printed, logged) = crate::testing::printed(&out);
        (out.status, printed, logged)
    }

    /// Closing the window suspends the title (its routines run to their
    /// end, told that the title is quiesced) and then ends the process
    /// with code 0; the title itself never hears of the close.
    #[test]
    fn close_suspends_then_exits() {
        let (status, printed, logged) = child("close");
        let all = format!("{printed}{logged}");
        assert!(
            printed.contains("CLOSING registered=0 window=true"),
            "{all}"
        );
        assert!(printed.contains("CLOSE RETURNED 0"), "{all}");
        assert!(printed.contains("SUSPENDED quiesced=1"), "{all}");
        assert!(!printed.contains("STILL RUNNING"), "{all}");
        assert_eq!(status.code(), Some(0), "{all}");
        assert!(
            logged.contains("window closed: suspending the title routines=1"),
            "{all}"
        );
        assert!(
            logged.contains("title suspended; ending the title"),
            "{all}"
        );
        assert!(!logged.contains("did not finish suspending"), "{all}");
    }

    /// A routine that outlasts the deadline does not keep the title
    /// alive: the process ends, with code 0, before the routine is done.
    #[test]
    fn a_slow_suspend_routine_is_cut_off_at_the_deadline() {
        assert!(
            SUSPEND_DEADLINE < Duration::from_secs(3),
            "the routine must be slower"
        );
        let (status, printed, logged) = child("slow-close");
        let all = format!("{printed}{logged}");
        assert!(printed.contains("CLOSE RETURNED 0"), "{all}");
        assert!(!printed.contains("SUSPENDED"), "{all}");
        assert!(!printed.contains("STILL RUNNING"), "{all}");
        assert_eq!(status.code(), Some(0), "{all}");
        let late = "title did not finish suspending in time; ending the title";
        assert!(logged.contains(late), "{all}");
        assert!(!logged.contains("title suspended;"), "{all}");
    }

    /// A relaunch asks for our own executable with the arguments given
    /// and an inheritable handle of this process in its environment. The
    /// process is described, not started.
    #[test]
    fn a_relaunch_asks_for_us_again_with_a_handle_of_this_process() {
        use windows_sys::Win32::Foundation::{CloseHandle, GetHandleInformation, HANDLE};
        use windows_sys::Win32::System::Threading::GetProcessId;
        let mut asked = None;
        relaunch_with(vec!["run".into(), "G:/game.exe".into()], |command| {
            let args: Vec<_> = command.get_args().map(|a| a.to_owned()).collect();
            let handle = command
                .get_envs()
                .find(|(name, _)| *name == RELAUNCHED_FROM)
                .and_then(|(_, value)| value.map(|v| v.to_owned()));
            asked = Some((command.get_program().to_owned(), args, handle));
            Ok(0)
        });
        let (program, args, handle) = asked.expect("the process was asked for");
        assert_eq!(program, std::env::current_exe().unwrap());
        assert_eq!(args, ["run", "G:/game.exe"]);
        let handle = handle.expect("the handle is in the environment");
        let handle = handle.to_str().unwrap().parse::<usize>().unwrap() as HANDLE;
        let mut flags = 0;
        // SAFETY: the handle `relaunch_with` duplicated, closed here.
        unsafe {
            assert_eq!(GetProcessId(handle), std::process::id());
            assert_ne!(GetHandleInformation(handle, &mut flags), 0);
            assert_ne!(flags & 1, 0, "inheritable");
            CloseHandle(handle);
        }
    }

    /// The relaunched process goes on only once the one it replaces has
    /// ended, and takes the handle out of its environment.
    #[test]
    fn a_relaunched_process_waits_for_the_one_it_replaces() {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::{
            CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE,
        };
        let test = "modules::lifecycle::tests::lifecycle_child";
        let mut previous = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture", "--test-threads=1"])
            .env(PART, "previous")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut inherited: HANDLE = std::ptr::null_mut();
        // SAFETY: an inheritable duplicate of the child's process handle.
        let duplicated = unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                previous.as_raw_handle(),
                GetCurrentProcess(),
                &mut inherited,
                0,
                1,
                DUPLICATE_SAME_ACCESS,
            )
        };
        assert_ne!(duplicated, 0);
        let handle = (inherited as usize).to_string();
        let out = crate::testing::child(
            test,
            &[
                (PART, "relaunched".as_ref()),
                (RELAUNCHED_FROM, handle.as_ref()),
            ],
        );
        let ended = previous.try_wait().unwrap();
        // SAFETY: our duplicate.
        unsafe { CloseHandle(inherited) };
        let _ = previous.kill();
        let _ = previous.wait();
        let (printed, logged) = crate::testing::printed(&out);
        assert!(
            printed.contains("WAITED handle_left=false"),
            "{printed}{logged}"
        );
        assert!(
            ended.is_some(),
            "the new process went on beside the old one"
        );
    }
}
