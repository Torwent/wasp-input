use std::{
    cell::RefCell,
    ffi::{c_char, c_int, c_void},
    mem::transmute,
    ptr::null_mut,
    slice::from_raw_parts,
    thread::sleep,
    time::Duration,
};

use windows::{
    core::{s, BOOL, PCSTR},
    Win32::{
        Foundation::{
            CloseHandle, FALSE, HINSTANCE, HMODULE, HWND, LPARAM, POINT, TRUE, WAIT_OBJECT_0,
            WAIT_TIMEOUT, WPARAM,
        },
        Graphics::Gdi::ScreenToClient,
        System::{
            Diagnostics::Debug::WriteProcessMemory,
            LibraryLoader::{DisableThreadLibraryCalls, GetModuleHandleA, GetProcAddress},
            Memory::{
                VirtualAllocEx, VirtualFreeEx, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
            },
            Threading::{
                CreateRemoteThread, CreateThread, GetCurrentProcessId, OpenProcess,
                WaitForSingleObject, PROCESS_ALL_ACCESS, THREAD_CREATION_FLAGS,
            },
        },
        UI::{
            Input::KeyboardAndMouse::{
                EnableWindow, IsWindowEnabled, MapVirtualKeyA, VkKeyScanA, MAPVK_VK_TO_VSC,
            },
            WindowsAndMessaging::{
                EnumChildWindows, EnumWindows, GetClassNameW, GetCursorPos,
                GetWindowThreadProcessId, PostMessageW, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN,
                WM_LBUTTONUP, WM_MOUSEMOVE, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_USER,
            },
        },
    },
};

use crate::client::hooks::{start_thread, unhook_wgl_swap_buffers, unhook_wndproc};

use super::memory::{MemoryManager, MEMORY_MANAGER};

pub const WI_CONSOLE: u32 = WM_USER + 1;
pub const WI_MODIFIERS: u32 = WM_USER + 2;

#[no_mangle]
pub static mut MODULE: HMODULE = HMODULE(null_mut());

#[no_mangle]
pub extern "system" fn DllMain(
    hinst_dll: HINSTANCE,
    fdw_reason: u32,
    _lpv_reserved: *mut c_void,
) -> BOOL {
    unsafe { MODULE = HMODULE(hinst_dll.0) };

    let pid = unsafe { GetCurrentProcessId() };
    let hwnd = match get_jagrenderview(pid) {
        Some(hwnd) => hwnd,
        None => return TRUE,
    };

    match fdw_reason {
        1 => unsafe {
            let _ = DisableThreadLibraryCalls(hinst_dll.into());

            let mem_manager = MEMORY_MANAGER.lock().unwrap();

            if mem_manager.is_mapped() {
                println!("[WaspInput]: Console attached. PID: {:?}\r\n", pid);

                let _ = CreateThread(
                    Some(null_mut()),
                    0,
                    Some(start_thread),
                    Some(hwnd.0 as *mut c_void),
                    THREAD_CREATION_FLAGS(0),
                    Some(null_mut()),
                );
            }
        },
        0 => {
            println!("[WaspInput]: Detached.\r\n");
            unsafe {
                unhook_wndproc();
                unhook_wgl_swap_buffers();
            };
        }
        _ => (),
    }

    TRUE
}

pub unsafe fn get_proc_address(name: *const c_char) -> *mut c_void {
    let name_str = PCSTR::from_raw(name as *const u8);
    let func_ptr = GetProcAddress(MODULE, name_str);
    transmute(func_ptr)
}

pub unsafe fn inject(module_path: &str, pid: u32) -> bool {
    MemoryManager::create_map();

    let process_handle = match OpenProcess(PROCESS_ALL_ACCESS, false, pid) {
        Ok(process) => {
            if WaitForSingleObject(process, 0) != WAIT_TIMEOUT {
                eprintln!("[WaspInput]: Process is not alive.\r\n");
                CloseHandle(process).ok();
                return false;
            }

            process
        }
        Err(_) => {
            eprintln!("[WaspInput]: OpenProcess failed.\r\n");
            return false;
        }
    };

    let kernel32 = match GetModuleHandleA(s!("kernel32.dll")) {
        Ok(h) => h,
        Err(_) => {
            eprintln!("[WaspInput]: GetModuleHandleA failed.\r\n");
            CloseHandle(process_handle).ok();
            return false;
        }
    };

    let size = module_path.len() + 1;
    let remote_address = VirtualAllocEx(
        process_handle,
        None,
        size,
        MEM_COMMIT | MEM_RESERVE,
        PAGE_READWRITE,
    );

    if remote_address.is_null() {
        eprintln!("[WaspInput]: VirtualAllocEx failed.\r\n");
        CloseHandle(process_handle).ok();
        return false;
    }

    let mut buffer = module_path.as_bytes().to_vec();
    buffer.push(0); // null-terminate

    if WriteProcessMemory(
        process_handle,
        remote_address,
        buffer.as_ptr() as _,
        buffer.len(),
        None,
    )
    .is_err()
    {
        eprintln!("[WaspInput]: WriteProcessMemory failed.\r\n");
        CloseHandle(process_handle).ok();
        return false;
    }

    let load_library = GetProcAddress(kernel32, s!("LoadLibraryA"))
        .map(|addr| std::mem::transmute::<_, unsafe extern "system" fn(*mut c_void) -> u32>(addr));

    let load_library = match load_library {
        Some(f) => f,
        None => {
            eprintln!("[WaspInput]: GetProcAddress failed.\r\n");
            CloseHandle(process_handle).ok();
            return false;
        }
    };

    let thread = match CreateRemoteThread(
        process_handle,
        None,
        0,
        Some(load_library),
        Some(remote_address),
        0,
        None,
    ) {
        Ok(thread) => thread,
        Err(_) => {
            eprintln!("[WaspInput]: CreateRemoteThread failed\r\n");
            CloseHandle(process_handle).ok();
            return false;
        }
    };

    let wait_result = WaitForSingleObject(thread, 5000);

    CloseHandle(thread).ok();
    CloseHandle(process_handle).ok();
    let _ = VirtualFreeEx(process_handle, remote_address, 0, MEM_RELEASE);

    if wait_result != WAIT_OBJECT_0 {
        eprintln!("[WaspInput]: WaitForSingleObject timed out.\r\n");
        return false;
    }

    true
}

pub fn get_jagrenderview(pid: u32) -> Option<HWND> {
    thread_local! {
        static FOUND_HWND: RefCell<Option<HWND>> = RefCell::new(None);
    }

    unsafe extern "system" fn enum_child_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let _ = lparam;
        let mut class_name = [0u16; 256];
        GetClassNameW(hwnd, &mut class_name);
        let class_str = String::from_utf16_lossy(&class_name[..]);
        if class_str.trim_end_matches('\0') == "JagRenderView" {
            FOUND_HWND.with(|cell| {
                *cell.borrow_mut() = Some(hwnd);
            });
            return FALSE; // Stop enumeration
        }
        TRUE
    }

    unsafe extern "system" fn enum_windows_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let mut proc_id = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut proc_id));
        if proc_id != lparam.0 as u32 {
            return TRUE;
        }

        let mut class_name = [0u16; 256];
        GetClassNameW(hwnd, &mut class_name);
        let class_str = String::from_utf16_lossy(&class_name[..]);
        if class_str.trim_end_matches('\0') == "JagWindow" {
            let _ = EnumChildWindows(Some(hwnd), Some(enum_child_proc), LPARAM(0));
            // If found, stop enumerating windows
            let found = FOUND_HWND.with(|cell| cell.borrow().is_some());
            if found {
                return FALSE;
            }
        }

        TRUE
    }

    unsafe {
        let _ = EnumWindows(Some(enum_windows_proc), LPARAM(pid as isize));
        FOUND_HWND.with(|cell| *cell.borrow())
    }
}

//input
pub fn is_input_enabled(hwnd: u64) -> bool {
    unsafe { IsWindowEnabled(HWND(hwnd as *mut c_void)).as_bool() }
}

pub fn toggle_input(hwnd: u64, state: bool) -> bool {
    unsafe { EnableWindow(HWND(hwnd as *mut c_void), state).as_bool() }
}

pub fn open_console(hwnd: u64) {
    let hwnd = Some(HWND(hwnd as *mut c_void));
    let _ = unsafe { PostMessageW(hwnd, WI_CONSOLE, WPARAM(0), LPARAM(0)) };
}

//mouse
pub fn get_mouse_position(hwnd: u64) -> Option<POINT> {
    let mut point = POINT::default();
    unsafe {
        if !GetCursorPos(&mut point).is_err() {
            let hwnd = HWND(hwnd as *mut c_void);
            if ScreenToClient(hwnd, &mut point).as_bool() {
                return Some(point);
            }
        }
    }
    None
}

pub fn mouse_move(hwnd: u64, x: i32, y: i32) {
    let hwnd = HWND(hwnd as *mut c_void);
    let lparam = (y << 16) | x;
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_MOUSEMOVE, WPARAM(0), LPARAM(lparam as isize));
    }
}

pub fn lbutton(hwnd: u64, down: bool, x: i32, y: i32) {
    let hwnd = HWND(hwnd as *mut c_void);
    let lparam = (y << 16) | x;
    unsafe {
        let _ = PostMessageW(
            Some(hwnd),
            if down { WM_LBUTTONDOWN } else { WM_LBUTTONUP },
            WPARAM(0),
            LPARAM(lparam as isize),
        );
    }
}

pub fn mbutton(hwnd: u64, down: bool, x: i32, y: i32) {
    let hwnd = HWND(hwnd as *mut c_void);
    let lparam = (y << 16) | x;
    unsafe {
        let _ = PostMessageW(
            Some(hwnd),
            if down { WM_RBUTTONDOWN } else { WM_RBUTTONUP },
            WPARAM(0),
            LPARAM(lparam as isize),
        );
    }
}

pub fn rbutton(hwnd: u64, down: bool, x: i32, y: i32) {
    let hwnd = HWND(hwnd as *mut c_void);
    let lparam = (y << 16) | x;
    unsafe {
        let _ = PostMessageW(
            Some(hwnd),
            if down { WM_RBUTTONDOWN } else { WM_RBUTTONUP },
            WPARAM(0),
            LPARAM(lparam as isize),
        );
    }
}

pub fn scroll(hwnd: u64, down: bool, scrolls: i32, x: i32, y: i32) {
    //let hwnd = HWND(hwnd as *mut c_void);
    let lparam = (y << 16) | x;
    print!(
        "[WaspInput]: TODO: scroll direction: {}, hwnd: {}, scrolls: {}, lparam: {}\r\n",
        down, hwnd, scrolls, lparam
    );
}

//keyboard
pub fn key_down(hwnd: u64, vkey: i32) {
    let hwnd = HWND(hwnd as *mut c_void);
    unsafe {
        let key = vkey & 0xFF;
        let scancode = MapVirtualKeyA(key as u32, MAPVK_VK_TO_VSC);
        let lparam = 1 | (scancode << 16) | (0 << 24);
        let _ = PostMessageW(
            Some(hwnd),
            WM_KEYDOWN,
            WPARAM(key as usize),
            LPARAM(lparam as isize),
        );
    }
}

pub fn key_up(hwnd: u64, vkey: i32) {
    let hwnd = HWND(hwnd as *mut c_void);
    unsafe {
        let key = vkey & 0xFF;
        let scancode = MapVirtualKeyA(key as u32, MAPVK_VK_TO_VSC);
        let lparam = 1 | (scancode << 16) | (0 << 24) | (1 << 30) | (1 << 31);
        let _ = PostMessageW(
            Some(hwnd),
            WM_KEYUP,
            WPARAM(key as usize),
            LPARAM(lparam as isize),
        );
    }
}

fn key_press(hwnd: HWND, vkey: i32, duration: u64) {
    let key = vkey & 0xFF;
    let scancode = unsafe { MapVirtualKeyA(vkey as u32, MAPVK_VK_TO_VSC) };

    let lparam = 1 | (scancode << 16) | (0 << 24);
    let _ = unsafe {
        PostMessageW(
            Some(hwnd),
            WM_KEYDOWN,
            WPARAM(key as usize),
            LPARAM(lparam as isize),
        )
    };

    sleep(Duration::from_millis(duration as u64));

    let lparam = 1 | (scancode << 16) | (0 << 24) | (1 << 30) | (1 << 31);
    let _ = unsafe {
        PostMessageW(
            Some(hwnd),
            WM_KEYUP,
            WPARAM(key as usize),
            LPARAM(lparam as isize),
        )
    };
}

fn update_modifiers(hwnd: HWND, shift: bool, ctrl: bool, alt: bool) {
    if !shift && !ctrl && !alt {
        return;
    }

    let mut wparam: usize = 0;
    if shift {
        wparam |= 1 << 0;
    }
    if ctrl {
        wparam |= 1 << 1;
    }
    if alt {
        wparam |= 1 << 2;
    }

    let _ = unsafe { PostMessageW(Some(hwnd), WI_MODIFIERS, WPARAM(wparam), LPARAM(0)) };
}

fn get_key_modifiers(ch: i8) -> (i16, bool, bool, bool) {
    let key = unsafe { VkKeyScanA(ch) } as i16;
    let modifiers = ((key >> 8) & 0xFF) as u8;

    let shift = modifiers & 0x01 != 0;
    let ctrl = modifiers & 0x02 != 0;
    let alt = modifiers & 0x04 != 0;

    (key, shift, ctrl, alt)
}

pub fn keys_send(hwnd: u64, text: *mut c_char, len: c_int, sleeptimes: *mut c_int) {
    let hwnd = HWND(hwnd as *mut c_void);

    let text_chars = unsafe { from_raw_parts(text, len as usize) };
    let sleep_times = unsafe { from_raw_parts(sleeptimes, len as usize) };

    let (mut pshift, mut pctrl, mut palt) = (false, false, false); //previous

    for (_, (&ch, &time)) in text_chars.iter().zip(sleep_times.iter()).enumerate() {
        let (key, shift, ctrl, alt) = get_key_modifiers(ch);

        update_modifiers(hwnd, shift != pshift, ctrl != pctrl, alt != palt);
        key_press(hwnd, key as i32, time as u64);

        (pshift, pctrl, palt) = (shift, ctrl, alt);
    }

    update_modifiers(hwnd, pshift, pctrl, palt);
}
