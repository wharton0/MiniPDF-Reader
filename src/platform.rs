//! Platform-specific operations (Windows FFI / Shell / GDI printing).

use std::path::Path;

#[cfg(windows)]
use crate::log_line;

#[cfg(windows)]
#[link(name = "shell32")]
extern "system" {
    fn ShellExecuteW(
        hwnd: isize,
        lpoperation: *const u16,
        lpfile: *const u16,
        lpparameters: *const u16,
        lpdirectory: *const u16,
        nshowcmd: i32,
    ) -> isize;
    fn SHOpenWithDialog(hwnd: isize, info: *const OPENASINFO) -> i32;
}

#[cfg(windows)]
#[link(name = "ole32")]
extern "system" {
    fn CoInitializeEx(reserved: *const std::ffi::c_void, coinit: u32) -> i32;
    fn CoUninitialize();
}

#[cfg(windows)]
const OAIF_ALLOW_REGISTRATION: u32 = 0x00000001;
#[cfg(windows)]
const OAIF_EXEC: u32 = 0x00000004;

#[cfg(windows)]
#[repr(C)]
struct OPENASINFO {
    psz_file: *const u16,
    psz_class: *const u16,
    flags: u32,
}

#[cfg(windows)]
#[link(name = "comdlg32")]
extern "system" {
    fn PrintDlgW(pd: *mut PRINTDLGW) -> i32;
    fn CommDlgExtendedError() -> u32;
}

#[cfg(windows)]
#[link(name = "gdi32")]
extern "system" {
    pub fn StartDocW(hdc: isize, docinfo: *const DOCINFOW) -> i32;
    pub fn StartPage(hdc: isize) -> i32;
    pub fn EndPage(hdc: isize) -> i32;
    pub fn EndDoc(hdc: isize) -> i32;
    pub fn StretchDIBits(
        hdc: isize,
        xdest: i32,
        ydest: i32,
        wdest: i32,
        hdest: i32,
        xsrc: i32,
        ysrc: i32,
        wsrc: i32,
        hsrc: i32,
        bits: *const u8,
        info: *const BITMAPINFO,
        usage: u32,
        rop: u32,
    ) -> i32;
    pub fn DeleteDC(hdc: isize) -> i32;
    pub fn GetDeviceCaps(hdc: isize, index: i32) -> i32;
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    pub fn GlobalFree(hmem: isize) -> isize;
}

#[cfg(windows)]
const PD_PAGENUMS: u32 = 0x00000002;
#[cfg(windows)]
const PD_NOSELECTION: u32 = 0x00000004;
#[cfg(windows)]
const PD_RETURNDC: u32 = 0x00000100;
#[cfg(windows)]
const PD_NOWARNING: u32 = 0x00000080;
#[cfg(windows)]
const HORZRES: i32 = 8;
#[cfg(windows)]
const VERTRES: i32 = 10;
pub const SRCCOPY: u32 = 0x00CC0020;
pub const DIB_RGB_COLORS: u32 = 0;

#[cfg(windows)]
#[repr(C)]
struct PRINTDLGW {
    l_struct_size: u32,
    hwnd_owner: isize,
    h_dev_mode: isize,
    h_dev_names: isize,
    hdc: isize,
    flags: u32,
    n_from_page: u16,
    n_to_page: u16,
    n_min_page: u16,
    n_max_page: u16,
    n_copies: u16,
    h_instance: isize,
    l_cust_data: isize,
    lpfn_print_hook: *const std::ffi::c_void,
    lpfn_setup_hook: *const std::ffi::c_void,
    lp_print_template_name: *const u16,
    lp_setup_template_name: *const u16,
    h_print_template: isize,
    h_setup_template: isize,
}

#[cfg(windows)]
#[repr(C)]
pub struct DOCINFOW {
    pub cb_size: i32,
    pub doc_name: *const u16,
    pub output: *const u16,
    pub datatype: *const u16,
    pub fw_type: u32,
}

#[cfg(windows)]
#[repr(C)]
pub struct BITMAPINFOHEADER {
    pub bi_size: u32,
    pub bi_width: i32,
    pub bi_height: i32,
    pub bi_planes: u16,
    pub bi_bit_count: u16,
    pub bi_compression: u32,
    pub bi_size_image: u32,
    pub bi_x_pels_per_meter: i32,
    pub bi_y_pels_per_meter: i32,
    pub bi_clr_used: u32,
    pub bi_clr_important: u32,
}

#[cfg(windows)]
#[repr(C)]
pub struct BITMAPINFO {
    pub header: BITMAPINFOHEADER,
}

/// Result of the modal PrintDlg: the printer DC + what to print.
pub struct PrintDialogResult {
    pub hdc: isize,
    pub h_dev_mode: isize,
    pub h_dev_names: isize,
    pub pages: Vec<i32>,
    pub copies: usize,
    pub printer_w: i32,
    pub printer_h: i32,
}

#[cfg(windows)]
pub fn show_print_dialog(total_pages: i32) -> Result<PrintDialogResult, String> {
    let total = total_pages.max(1);
    let handle = std::thread::Builder::new()
        .name("minipdf-printdlg".into())
        .spawn(move || unsafe { show_print_dialog_impl(total) })
        .map_err(|e| format!("spawn failed: {e}"))?;
    handle
        .join()
        .map_err(|_| "print dialog thread panicked".to_owned())?
}

#[cfg(windows)]
unsafe fn show_print_dialog_impl(total_pages: i32) -> Result<PrintDialogResult, String> {
    const COINIT_APARTMENTTHREADED: u32 = 0x2;
    let _ = CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED);

    let mut pd: PRINTDLGW = std::mem::zeroed();
    pd.l_struct_size = std::mem::size_of::<PRINTDLGW>() as u32;
    pd.flags = PD_RETURNDC | PD_NOSELECTION | PD_NOWARNING;
    pd.n_min_page = 1;
    pd.n_max_page = total_pages.min(65535) as u16;
    pd.n_from_page = 1;
    pd.n_to_page = total_pages.min(65535) as u16;

    let ok = PrintDlgW(&mut pd);
    if ok == 0 {
        let cderr = CommDlgExtendedError();
        log_line(&format!(
            "PrintDlgW failed: CommDlgExtendedError=0x{:08X}",
            cderr
        ));
        if pd.hdc != 0 {
            DeleteDC(pd.hdc);
        }
        if pd.h_dev_mode != 0 {
            GlobalFree(pd.h_dev_mode);
        }
        if pd.h_dev_names != 0 {
            GlobalFree(pd.h_dev_names);
        }
        return Err(if cderr == 0 {
            "Print cancelled".to_owned()
        } else {
            format!("Print dialog error 0x{:08X}", cderr)
        });
    }

    let pages: Vec<i32> = if pd.flags & PD_PAGENUMS != 0 && pd.n_from_page > 0 && pd.n_to_page > 0 {
        let from = (pd.n_from_page as i32 - 1).max(0);
        let to = (pd.n_to_page as i32 - 1).min(total_pages - 1);
        (from..=to).collect()
    } else {
        (0..total_pages).collect()
    };

    if pages.is_empty() || pd.hdc == 0 {
        if pd.hdc != 0 {
            DeleteDC(pd.hdc);
        }
        if pd.h_dev_mode != 0 {
            GlobalFree(pd.h_dev_mode);
        }
        if pd.h_dev_names != 0 {
            GlobalFree(pd.h_dev_names);
        }
        return Err("No pages to print".to_owned());
    }

    let copies = pd.n_copies.max(1) as usize;
    let printer_w = GetDeviceCaps(pd.hdc, HORZRES);
    let printer_h = GetDeviceCaps(pd.hdc, VERTRES);

    Ok(PrintDialogResult {
        hdc: pd.hdc,
        h_dev_mode: pd.h_dev_mode,
        h_dev_names: pd.h_dev_names,
        pages,
        copies,
        printer_w,
        printer_h,
    })
}

#[cfg(not(windows))]
pub fn show_print_dialog(_total_pages: i32) -> Result<PrintDialogResult, String> {
    Err("Printing is not supported on this platform".to_owned())
}

#[cfg(windows)]
pub fn shell_verb(path: &Path, verb: &str) -> Result<(), String> {
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
    let op = wide(verb);
    let file = wide(&path.to_string_lossy());
    let ret = unsafe {
        ShellExecuteW(
            0,
            op.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1, // SW_SHOWNORMAL
        )
    };
    if ret > 32 {
        Ok(())
    } else {
        Err(match ret as i32 {
            0 | 8 => "Out of memory".to_owned(),
            2 => "File not found".to_owned(),
            3 => "Path not found".to_owned(),
            5 => "Access denied".to_owned(),
            11 => "Bad file format".to_owned(),
            27 => "File association is incomplete".to_owned(),
            29 => "Print handler failed".to_owned(),
            30 => "Print handler is busy".to_owned(),
            31 => "No app handles this file (set a default PDF reader)".to_owned(),
            32 => "Required DLL not found".to_owned(),
            c => format!("System error {c}"),
        })
    }
}

#[cfg(not(windows))]
pub fn shell_verb(_path: &Path, _verb: &str) -> Result<(), String> {
    Err("One-click print is not supported on this platform".to_owned())
}

#[cfg(windows)]
pub fn shell_openas_com(path: &Path) -> Result<(), String> {
    let path = path.to_string_lossy().to_string();
    let handle = std::thread::Builder::new()
        .name("minipdf-openas".into())
        .spawn(move || unsafe { shell_openas_com_impl(&path) })
        .map_err(|e| format!("spawn failed: {e}"))?;
    handle
        .join()
        .map_err(|_| "openas thread panicked".to_owned())?
}

#[cfg(windows)]
unsafe fn shell_openas_com_impl(path: &str) -> Result<(), String> {
    const COINIT_APARTMENTTHREADED: u32 = 0x2;
    let _ = CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED);

    let file_w: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let info = OPENASINFO {
        psz_file: file_w.as_ptr(),
        psz_class: std::ptr::null(),
        flags: OAIF_ALLOW_REGISTRATION | OAIF_EXEC,
    };
    let hr = SHOpenWithDialog(0, &info);
    CoUninitialize();

    if hr >= 0 {
        Ok(())
    } else {
        Err(format!("SHOpenWithDialog failed: 0x{:08X}", hr as u32))
    }
}

#[cfg(not(windows))]
pub fn shell_openas_com(_path: &Path) -> Result<(), String> {
    Err("Not supported on this platform".to_owned())
}
