use detour::static_detour;
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use std::{
    ffi::{c_void, CStr, CString},
    intrinsics::transmute,
    mem::{size_of_val, MaybeUninit},
    ptr,
    slice::{from_raw_parts, from_raw_parts_mut},
    sync::Mutex,
};
use widestring::{U16CString, WideCString};
use windows_sys::{
    core::{PCWSTR, PWSTR},
    Win32::{
        Foundation::{GetLastError, BOOL, HANDLE, WIN32_ERROR},
        Security::SECURITY_ATTRIBUTES,
        System::{
            Diagnostics::Debug::{WriteProcessMemory, PROCESSOR_ARCHITECTURE_INTEL},
            LibraryLoader::{GetModuleFileNameW, GetProcAddress, LoadLibraryW},
            Memory::{VirtualAllocEx, MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READWRITE},
            // 在你添加了 feature 之后，你在 vscode 中输入类型名称后，一般会有提示，回车之后会自动添加这一条 use。
            // 有时候可能不会出现，多输入几个字符，甚至完全输入之后应该会出现。另外，可以完全输入之后，在报错的地方悬浮，点击 quick fix 然后导入也可。
            Registry::{HKEY, REG_VALUE_TYPE},
            SystemInformation::{
                GetNativeSystemInfo, GetSystemDirectoryW, FIRMWARE_TABLE_ID,
                FIRMWARE_TABLE_PROVIDER, SYSTEM_INFO,
            },
            Threading::{
                CreateRemoteThread, GetCurrentProcess, GetExitCodeThread, GetThreadId,
                IsWow64Process, ResumeThread, TerminateProcess, WaitForSingleObject,
                CREATE_SUSPENDED, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION, STARTUPINFOW,
            },
        },
    },
};

use crate::config::{get_firmware_config, Config};

#[derive(Serialize, Deserialize, Clone)]
pub struct InjectOptions {
    pub server_address: Option<String>,
    pub inject_sub_process: bool,
    pub includes_system_process: bool,
}

#[repr(C)]
#[derive(Serialize, Deserialize)]
pub struct INJECT_OPTIONS_WRAPPER {
    pub len: usize,
    pub ptr: u64,
}

#[repr(C)]
#[derive(Clone)]
#[allow(non_snake_case)]
pub struct RawSMBIOSData {
    pub Used20CallingMethod: u8,
    pub SMBIOSMajorVersion: u8,
    pub SMBIOSMinorVersion: u8,
    pub DmiRevision: u8,
    pub Length: u32,
    pub SMBIOSTableData: [u8; 0],
}

#[repr(C)]
#[derive(Clone, Default)]
#[allow(non_snake_case)]
pub struct SMBIOSHEADER {
    pub Type: u8,
    pub Length: u8,
    pub Handle: u16,
}

#[repr(C)]
#[derive(Clone, Default)]
#[allow(non_snake_case)]
pub struct SystemInfo {
    pub Header: SMBIOSHEADER,
    pub Manufacturer: u8,
    pub ProductName: u8,
    pub Version: u8,
    pub SN: u8,
    pub UUID: [u8; 16],
    pub WakeUpType: u8,
    pub SKUNumber: u8,
    pub Family: u8,
}

type FnCreateProcessW = unsafe extern "system" fn(
    PCWSTR,
    PWSTR,
    *const SECURITY_ATTRIBUTES,
    *const SECURITY_ATTRIBUTES,
    BOOL,
    PROCESS_CREATION_FLAGS,
    *const c_void,
    PCWSTR,
    *const STARTUPINFOW,
    *mut PROCESS_INFORMATION,
) -> BOOL;
type FnGetSystemFirmwareTable = unsafe extern "system" fn(
    FIRMWARE_TABLE_PROVIDER,
    FIRMWARE_TABLE_ID,
    *mut ::core::ffi::c_void,
    u32,
) -> u32;
type FnHookRegQueryValueExW = unsafe extern "system" fn(
    HKEY,
    PCWSTR,
    *mut u32,
    *mut REG_VALUE_TYPE,
    *mut u8,
    *mut u32,
) -> WIN32_ERROR;

static_detour! {
    static HookCreateProcessW: unsafe extern "system" fn(
        PCWSTR,
        PWSTR,
        *const SECURITY_ATTRIBUTES,
        *const SECURITY_ATTRIBUTES,
        BOOL,
        PROCESS_CREATION_FLAGS,
        *const c_void,
        PCWSTR,
        *const STARTUPINFOW,
        *mut PROCESS_INFORMATION
    ) -> BOOL;

  static HookGetSystemFirmwareTable: unsafe extern "system" fn(
        u32,
        u32,
        *mut c_void,
        u32
    ) -> u32;

    // 优先使用 W 系列函数，Windows 中 A 系列函数一般是 ANSI 编码，而 Rust 是 UTF-8 编码，会导致乱码。
    // 对于函数的签名，先在 windows-sys 这个 crate 里面搜索，然后看一下需要哪些 features，将其添加到 Cargo.toml 中。
    static HookRegQueryValueExW: unsafe extern "system" fn(
        HKEY,
        PCWSTR,
        *mut u32,
        *mut REG_VALUE_TYPE,
        *mut u8,
        *mut u32 // 最后位置不可加多余的逗号，static_detour 解析会出错（不需要加 recursion_limit）。
    ) -> WIN32_ERROR;

    // static HookRegQueryValueExA: unsafe extern "system" fn(
    //     HKEY,
    //     PCSTR,
    //     PDWORD,
    //     PDWORD,
    //     PBYTE,
    //     PDWORD,
    // ) -> LSTATUS;
}

static LIBRARY_NAME: &str = "huawei_pc_manager_bootstrap_core.dll";
static SMBIOS_FIRMWARE_TABLE_PROVIDER: u32 = 1381190978;
static SMBIOS_FIRMWARE_TABLE_ID: u32 = 0;

lazy_static::lazy_static! {
    static ref CUSTOM_SMBIOS_BUFFER: Mutex<Option<Vec<u8>>> = Mutex::new(None);
}

fn is_args_can_hook(
    firmwaretableprovidersignature: FIRMWARE_TABLE_PROVIDER,
    firmwaretableid: FIRMWARE_TABLE_ID,
) -> bool {
    firmwaretableprovidersignature == SMBIOS_FIRMWARE_TABLE_PROVIDER
        && firmwaretableid == SMBIOS_FIRMWARE_TABLE_ID
}

fn get_and_cache_firmware_table() -> anyhow::Result<()> {
    if CUSTOM_SMBIOS_BUFFER
        .lock()
        .map_err(|err| anyhow::anyhow!("Failed to lock custom smbios data buffer: {}", err))?
        .is_some()
    {
        info!("Using cached custom smbios data");
        return Ok(());
    }

    let buffer_size = unsafe {
        HookGetSystemFirmwareTable.call(
            SMBIOS_FIRMWARE_TABLE_PROVIDER,
            SMBIOS_FIRMWARE_TABLE_ID,
            std::ptr::null_mut::<c_void>(),
            0,
        )
    };
    if buffer_size == 0 {
        return Err(anyhow::anyhow!(
            "Failed to get firmware table: {}",
            std::io::Error::last_os_error()
        ));
    }

    let mut buffer: Vec<u8> = vec![0; buffer_size as usize];
    let buffer_size = unsafe {
        HookGetSystemFirmwareTable.call(
            SMBIOS_FIRMWARE_TABLE_PROVIDER,
            SMBIOS_FIRMWARE_TABLE_ID,
            buffer.as_mut_ptr() as *mut c_void,
            buffer.len() as u32,
        )
    };
    if buffer_size == 0 {
        return Err(anyhow::anyhow!(
            "Failed to get firmware table: {}",
            std::io::Error::last_os_error()
        ));
    }

    CUSTOM_SMBIOS_BUFFER
        .lock()
        .map_err(|err| anyhow::anyhow!("Failed to lock custom smbios data buffer: {}", err))?
        .replace(replace_smbios_manufacturer(buffer));

    Ok(())
}

fn replace_smbios_manufacturer(mut smbios_data: Vec<u8>) -> Vec<u8> {
    unsafe {
        let raw_bios_ptr = smbios_data.as_mut_ptr() as *mut RawSMBIOSData;
        let start_entry_ptr: *mut u8 =
            &(*raw_bios_ptr).SMBIOSTableData as *const [u8; 0] as *mut u8;
        let end_ptr = start_entry_ptr.add((*raw_bios_ptr).Length as usize);
        let mut header_ptr: *mut SMBIOSHEADER = transmute(start_entry_ptr as *mut u8);
        let mut smbios_entry_list: Vec<Vec<u8>> = vec![];

        while (header_ptr as usize) < end_ptr as usize {
            let mut next_header = (header_ptr as *const u8).offset((*header_ptr).Length.into());
            while 0 != (*next_header | *(next_header.offset(1)))
                && (next_header as usize) < end_ptr as usize
            {
                next_header = next_header.offset(1);
            }
            next_header = next_header.offset(2);

            let header_length =
                std::cmp::min(next_header as usize, end_ptr as usize) - (header_ptr as usize);
            if header_length > 0 {
                smbios_entry_list
                    .push(from_raw_parts(header_ptr as *const u8, header_length).to_vec());
            }

            header_ptr = next_header as *mut SMBIOSHEADER;
        }

        let mut custom_smbios_data =
            smbios_data[..start_entry_ptr as usize - raw_bios_ptr as usize].to_vec();
        custom_smbios_data.append(
            &mut smbios_entry_list
                .into_iter()
                .map(|entry| {
                    if entry.first() == Some(&1) {
                        let new_sys_entry = construct_own_sys_info(
                            &get_firmware_config().unwrap_or_else(|_| Config::default()),
                        );
                        dump_sys_info(&*(new_sys_entry.as_ptr() as *const SystemInfo));
                        new_sys_entry
                    } else {
                        entry
                    }
                })
                .fold(vec![], |mut vec, mut entry| {
                    vec.append(&mut entry);
                    vec
                }),
        );

        custom_smbios_data
    }
}
fn detour_reg_query_value(
    // see: https://docs.rs/windows-sys/latest/windows_sys/Win32/System/Registry/fn.RegQueryValueExW.html
    // [in]                HKEY    hKey,
    // [in, optional]      LPCSTR  lpValueName,
    //                     LPDWORD lpReserved,
    // [out, optional]     LPDWORD lpType,
    // [out, optional]     LPBYTE  lpData,
    // [in, out, optional] LPDWORD lpcbData
    hkey: HKEY,
    lpvaluename: PCWSTR,
    lpreserved: *mut u32,
    lptype: *mut REG_VALUE_TYPE,
    lpdata: *mut u8,
    lpcbdata: *mut u32,
) -> WIN32_ERROR {
    info!(
        "Calling ReqQueryValue: {}, {}, {}, {:?}, {}, {}",
        hkey,
        lpvaluename as usize,
        lpreserved as usize,
        unsafe { lptype.as_ref() },
        lpdata as usize,
        lpcbdata as usize
    );
    unsafe { HookRegQueryValueExW.call(hkey, lpvaluename, lpreserved, lptype, lpdata, lpcbdata) }
}
fn detour_get_system_firmware_table(
    firmwaretableprovidersignature: FIRMWARE_TABLE_PROVIDER,
    firmwaretableid: FIRMWARE_TABLE_ID,
    pfirmwaretablebuffer: *mut ::core::ffi::c_void,
    buffersize: u32,
) -> u32 {
    let sig_name = get_firmware_table_provider_signature(firmwaretableprovidersignature);
    let id_name = get_firmware_table_provider_signature(firmwaretableid);
    info!(
        "Calling GetSystemFirmwareTable: {}({}), {}({}), 0x{:x}, {}",
        firmwaretableprovidersignature,
        sig_name,
        firmwaretableid,
        id_name,
        pfirmwaretablebuffer as usize,
        buffersize
    );

    if is_args_can_hook(firmwaretableprovidersignature, firmwaretableid) {
        let cache_smbios_buffer_result = get_and_cache_firmware_table();
        if cache_smbios_buffer_result.is_ok() {
            match CUSTOM_SMBIOS_BUFFER.lock() {
                Ok(buffer) => {
                    if let Some(buffer) = buffer.as_ref() {
                        if pfirmwaretablebuffer as usize == 0 {
                            info!("Need {} bytes buffer", buffer.len());
                            return buffer.len() as u32;
                        }
                        let dest: &mut [u8] = unsafe {
                            from_raw_parts_mut(pfirmwaretablebuffer as *mut u8, buffersize as usize)
                        };
                        let min_size = std::cmp::min(dest.len(), buffer.len());
                        dest.copy_from_slice(&buffer[0..min_size]);
                        info!("Copied {} bytes from buffer", min_size);
                        return min_size as u32;
                    }
                    error!("Failed to get custom smbios buffer");
                }
                Err(err) => {
                    error!("Failed to lock custom smbios data buffer: {}", err);
                }
            }
        } else {
            error!(
                "Failed to get and cache firmware table: {}",
                cache_smbios_buffer_result.unwrap_err()
            );
        }
        return 0;
    }

    unsafe {
        HookGetSystemFirmwareTable.call(
            firmwaretableprovidersignature,
            firmwaretableid,
            pfirmwaretablebuffer,
            buffersize,
        )
    }
}

fn dump_sys_info(sys_info: &SystemInfo) {
    let first_str_ptr = unsafe {
        (sys_info as *const SystemInfo as *const u8).add(sys_info.Header.Length as usize)
    };
    info!(
        "Manufacturer: {}",
        locate_string(first_str_ptr, sys_info.Manufacturer)
            .unwrap_or_else(|| String::from("No Manufacturer"))
    );
    info!(
        "ProductName: {}",
        locate_string(first_str_ptr, sys_info.ProductName)
            .unwrap_or_else(|| String::from("No ProductName"))
    );
    info!(
        "Version: {}",
        locate_string(first_str_ptr, sys_info.Version)
            .unwrap_or_else(|| String::from("No Version"))
    );
    info!(
        "SN: {}",
        locate_string(first_str_ptr, sys_info.SN).unwrap_or_else(|| String::from("No SN"))
    );
}

fn construct_own_sys_info(config: &Config) -> Vec<u8> {
    let sys_info_data = format!(
        "{}\0{}\0{}\0{}\0{}\0\0",
        config.manufacturer, config.product_name, config.version, config.sn, config.sku
    );

    let mut sys_info = SystemInfo::default();

    sys_info.Header.Length = std::mem::size_of_val(&sys_info) as u8;
    sys_info.Header.Type = 1;
    sys_info.Header.Handle = 1;

    sys_info.Manufacturer = 1;
    sys_info.ProductName = 2;
    sys_info.Version = 3;
    sys_info.SN = 4;
    sys_info.SKUNumber = 5;

    sys_info.WakeUpType = 0;
    sys_info.SKUNumber = 0;
    sys_info.Family = 0;
    sys_info.UUID = [0; 16];

    let mut entry_data = vec![];

    entry_data.append(&mut unsafe {
        from_raw_parts(
            &sys_info as *const SystemInfo as *const u8,
            std::mem::size_of_val(&sys_info),
        )
        .to_vec()
    });
    entry_data.append(&mut sys_info_data.as_bytes().to_vec());
    entry_data
}

fn locate_string(oem_str: *const u8, index: u8) -> Option<String> {
    if index == 0 || unsafe { *oem_str } == 0 {
        return None;
    }
    let mut i = index;
    let mut str_ptr = oem_str;
    loop {
        i -= 1;
        if i == 0 {
            break;
        }
        str_ptr = unsafe { str_ptr.add(str_len(str_ptr) as usize + 1) }
    }
    Some(
        unsafe { CStr::from_ptr(str_ptr as *const i8) }
            .to_str()
            .unwrap()
            .to_string(),
    )
}

fn str_len(cstr: *const u8) -> usize {
    let mut current_ptr = cstr;
    let mut count = 0;
    while unsafe { *current_ptr != 0 } {
        count += 1;
        current_ptr = unsafe { current_ptr.offset(1) };
    }
    count
}

#[allow(clippy::too_many_arguments)]
fn detour_create_process(
    opts: &Option<InjectOptions>,
    app_name: PCWSTR,
    cmd_line: PWSTR,
    proc_attrs: *const SECURITY_ATTRIBUTES,
    th_attrs: *const SECURITY_ATTRIBUTES,
    inherit: BOOL,
    flags: PROCESS_CREATION_FLAGS,
    env: *const c_void,
    cur_dir: PCWSTR,
    startup_info: *const STARTUPINFOW,
    proc_info: *mut PROCESS_INFORMATION,
) -> BOOL {
    unsafe {
        let app_name_string = if app_name.is_null() {
            String::new()
        } else {
            U16CString::from_ptr_str(app_name).to_string().unwrap()
        };
        let cmd_line_string = if cmd_line.is_null() {
            String::new()
        } else {
            U16CString::from_ptr_str(cmd_line).to_string().unwrap()
        };
        info!("CreateProcessW: {} {}", app_name_string, cmd_line_string);

        let flags_with_suspend = CREATE_SUSPENDED | flags;
        let creating_res = HookCreateProcessW.call(
            app_name,
            cmd_line,
            proc_attrs,
            th_attrs,
            inherit,
            flags_with_suspend,
            env,
            cur_dir,
            startup_info,
            proc_info,
        );

        if creating_res != 0 {
            info!("New process id: {:?}", (*proc_info).dwProcessId);
            if cmd_line_string.contains("isSupportDevice") {
                info!("Command line contains isSupportDevice, exit with 1");
                TerminateProcess((*proc_info).hProcess, 1);
                return creating_res;
            }
            if cmd_line_string.contains("IsSupportBaZhang") {
                info!("Command line contains IsSupportBaZhang, exit with 2");
                TerminateProcess((*proc_info).hProcess, 2);
                return creating_res;
            }
            let should_inject = opts
                .as_ref()
                .map(|opts| {
                    should_inject_process(opts, app_name_string.as_str(), cmd_line_string.as_str())
                })
                .unwrap_or(true);
            if should_inject {
                if let Err(err) = inject_to_process((*proc_info).hProcess, opts) {
                    warn!("inject_to_process error: {}", err);
                }
            } else {
                info!("Skip system process.");
            }
            if flags & CREATE_SUSPENDED == 0 && ResumeThread((*proc_info).hThread) == u32::MAX {
                warn!("ResumeThread error: {}", GetLastError());
            }
        } else {
            warn!("CreateProcessW failed: {}", GetLastError());
        }

        creating_res
    }
}

pub fn enable_hook(opts: Option<InjectOptions>) -> anyhow::Result<()> {
    let inject_sub_process = opts
        .as_ref()
        .map(|opts| opts.inject_sub_process)
        .unwrap_or(false);
    unsafe {
        let fp_create_process: FnCreateProcessW =
            transmute(get_proc_address("CreateProcessW", "kernel32.dll")?);
        info!("Got CreateProcessW: 0x{:x}", fp_create_process as usize);

        let fp_get_system_firmware_table: FnGetSystemFirmwareTable =
            transmute(get_proc_address("GetSystemFirmwareTable", "kernel32.dll")?);
        info!(
            "Got GetSystemFirmwareTable: 0x{:x}",
            fp_get_system_firmware_table as usize
        );

        let fp_reg_query_value: FnHookRegQueryValueExW =
            transmute(get_proc_address("RegQueryValueExW", "Advapi32.dll")?);
        info!("Got RegQueryValueExW: 0x{:x}", fp_reg_query_value as usize);

        let opts = Box::leak(Box::new(opts));
        HookGetSystemFirmwareTable.initialize(
            fp_get_system_firmware_table,
            detour_get_system_firmware_table,
        )?;
        info!("HookGetSystemFirmwareTable initialized");
        HookRegQueryValueExW.initialize(
            fp_reg_query_value,
            |hkey, value_name, reserved, typem, data, pcbdata| {
                detour_reg_query_value(hkey, value_name, reserved, typem, data, pcbdata)
            },
        )?;
        info!("HookRegQueryValueExA initialized");
        HookCreateProcessW.initialize(
            fp_create_process,
            |app_name,
             cmd_line,
             proc_attrs,
             th_attrs,
             inherit,
             flags,
             env,
             cur_dir,
             startup_info,
             proc_info| {
                detour_create_process(
                    opts,
                    app_name,
                    cmd_line,
                    proc_attrs,
                    th_attrs,
                    inherit,
                    flags,
                    env,
                    cur_dir,
                    startup_info,
                    proc_info,
                )
            },
        )?;
        info!("HookCreateProcessW initialized");
        HookGetSystemFirmwareTable.enable()?;
        info!("HookGetSystemFirmwareTable enabled");

        if inject_sub_process {
            HookCreateProcessW.enable()?;
            info!("HookCreateProcessW enabled");
        }
    }

    Ok(())
}

fn should_inject_process(opts: &InjectOptions, app_name: &str, cmd_line: &str) -> bool {
    opts.includes_system_process
        || (if !app_name.trim().is_empty() {
            !check_path_is_system(app_name)
        } else {
            !check_path_is_system(cmd_line)
        })
}

unsafe fn get_proc_address(
    proc_name: &str,
    module_name: &str,
) -> anyhow::Result<unsafe extern "system" fn() -> isize> {
    let module_name_cstr = WideCString::from_str(module_name)?;
    let h_inst = LoadLibraryW(module_name_cstr.as_ptr());
    if h_inst == 0 {
        return Err(anyhow::anyhow!(
            "LoadLibraryW failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    let proc_name_cstr = CString::new(proc_name)?;
    GetProcAddress(h_inst, proc_name_cstr.as_ptr() as _).ok_or_else(|| {
        anyhow::anyhow!("GetProcAddress failed: {}", std::io::Error::last_os_error())
    })
}

fn check_path_is_system(path: &str) -> bool {
    let mut path_buffer = [0; 4096];
    let size = unsafe { GetSystemDirectoryW(path_buffer.as_mut_ptr(), path_buffer.len() as _) };
    if size > 0 {
        let sys_dir = unsafe { WideCString::from_ptr(path_buffer.as_ptr(), size as _) }
            .ok()
            .and_then(|s| s.to_string().ok());
        info!("System directory: {:?}", sys_dir);
        if let Some(sys_dir) = sys_dir {
            let slash_sys_dir = sys_dir.replace('\\', "/").to_ascii_lowercase();
            let slash_path = path.replace('\\', "/").to_ascii_lowercase();
            return slash_path.starts_with(&slash_sys_dir)
                || (slash_path.chars().next() == Some('"')
                    && slash_path[1..].starts_with(&slash_sys_dir));
        }
    }
    false
}

unsafe fn inject_to_process(
    process_handle: HANDLE,
    opts: &Option<InjectOptions>,
) -> anyhow::Result<()> {
    let is_target_x86 = is_process_x86(process_handle)?;
    let is_self_x86 = is_process_x86(GetCurrentProcess())?;
    if is_target_x86 != is_self_x86 {
        return Err(anyhow::anyhow!(
            "Process architecture mismatch, expect {} got {}",
            if is_self_x86 { "x86" } else { "x64" },
            if is_target_x86 { "x86" } else { "x64" }
        ));
    }

    let mut lib_full_path = std::env::current_exe()?
        .parent()
        .ok_or_else(|| anyhow::anyhow!("No path content"))?
        .to_path_buf();
    lib_full_path.push(LIBRARY_NAME);
    let lib_full_path = lib_full_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("No path content"))?;
    info!("Get enable_hook address from {}", lib_full_path);
    let fp_enable_hook = get_proc_address("enable_hook", lib_full_path)?;

    let library_name_with_null = WideCString::from_str(LIBRARY_NAME)?;
    let core_module_handle = LoadLibraryW(library_name_with_null.as_ptr() as _);
    let mut core_full_name_buffer = [0; 4096];
    if core_module_handle == 0
        || GetModuleFileNameW(
            core_module_handle,
            core_full_name_buffer.as_mut_ptr(),
            core_full_name_buffer.len() as u32,
        ) == 0
    {
        return Err(anyhow::anyhow!(
            "GetModuleFileNameA failed: {}",
            GetLastError()
        ));
    }
    let library_name_addr = write_process_memory(
        process_handle,
        std::slice::from_raw_parts(
            core_full_name_buffer.as_ptr() as _,
            size_of_val(&core_full_name_buffer),
        ),
    )?;
    let fp_load_library = get_proc_address("LoadLibraryW", "kernel32.dll")?;
    let load_library_thread = CreateRemoteThread(
        process_handle,
        ptr::null(),
        0,
        Some(transmute(fp_load_library)),
        library_name_addr,
        0,
        ptr::null_mut(),
    );
    if load_library_thread == 0 {
        return Err(anyhow::anyhow!(
            "CreateRemoteThread failed: {}",
            GetLastError()
        ));
    }
    info!(
        "Created LoadLibraryW thread with id: {}",
        GetThreadId(load_library_thread)
    );
    let wait_result = WaitForSingleObject(load_library_thread, 0xFFFFFFFF);
    if wait_result != 0 {
        return Err(anyhow::anyhow!(
            "WaitForSingleObject failed: {}",
            wait_result
        ));
    }
    let mut module_handle: u32 = 0;
    if GetExitCodeThread(load_library_thread, &mut module_handle as *mut u32) != 0
        && module_handle == 0
    {
        return Err(anyhow::anyhow!("Remote LoadLibraryW failed"));
    }

    let enable_hook_params = if let Some(opts) = opts {
        let opts_bytes = bincode::serialize(opts)?;
        let opts_ptr = write_process_memory(process_handle, opts_bytes.as_slice())?;
        info!("Write options to address {:?}", opts_ptr);
        let opts_wrapper = INJECT_OPTIONS_WRAPPER {
            len: opts_bytes.len(),
            ptr: opts_ptr as u64,
        };
        let opts_wrapper_bytes = bincode::serialize(&opts_wrapper)?;
        let opts_wrapper_ptr = write_process_memory(process_handle, opts_wrapper_bytes.as_slice())?;
        info!("Write options wrapper to address {:?}", opts_wrapper_ptr);
        opts_wrapper_ptr
    } else {
        ptr::null()
    };
    let thread_handle = CreateRemoteThread(
        process_handle,
        ptr::null(),
        0,
        Some(transmute(fp_enable_hook)),
        enable_hook_params,
        0,
        ptr::null_mut(),
    );
    if thread_handle == 0 {
        return Err(anyhow::anyhow!(
            "CreateRemoteThread failed: {}",
            GetLastError()
        ));
    }
    info!(
        "Created enable_hook thread with id: {}",
        GetThreadId(thread_handle)
    );
    let wait_result = WaitForSingleObject(thread_handle, 0xFFFFFFFF);
    if wait_result != 0 {
        return Err(anyhow::anyhow!(
            "WaitForSingleObject failed: {}",
            wait_result
        ));
    }

    Ok(())
}

fn is_process_x86(process_handle: HANDLE) -> anyhow::Result<bool> {
    let sys_info = unsafe {
        let mut sys_info = MaybeUninit::<SYSTEM_INFO>::uninit();
        GetNativeSystemInfo(sys_info.as_mut_ptr());
        sys_info.assume_init()
    };
    let processor_arch = unsafe { sys_info.Anonymous.Anonymous.wProcessorArchitecture };
    Ok(processor_arch == PROCESSOR_ARCHITECTURE_INTEL || is_wow64_process(process_handle)?)
}

fn is_wow64_process(process_handle: HANDLE) -> anyhow::Result<bool> {
    let mut is_wow64 = 0;
    unsafe {
        if IsWow64Process(process_handle, &mut is_wow64) == 0 {
            return Err(anyhow::anyhow!("IsWow64Process failed: {}", GetLastError()));
        }
    }
    Ok(is_wow64 != 0)
}

unsafe fn write_process_memory(
    process_handle: HANDLE,
    content: &[u8],
) -> anyhow::Result<*mut c_void> {
    let target_address = VirtualAllocEx(
        process_handle,
        ptr::null(),
        content.len(),
        MEM_COMMIT | MEM_RESERVE,
        PAGE_EXECUTE_READWRITE,
    );
    if target_address.is_null() {
        return Err(anyhow::anyhow!("VirtualAllocEx failed: {}", GetLastError()));
    }
    let success = WriteProcessMemory(
        process_handle,
        target_address,
        content.as_ptr() as *const c_void,
        content.len(),
        ptr::null_mut(),
    );
    if success == 0 {
        return Err(anyhow::anyhow!(
            "WriteProcessMemory failed: {}",
            GetLastError()
        ));
    }
    Ok(target_address)
}

fn get_firmware_table_provider_signature(firmwaretableprovidersignature: u32) -> String {
    let mut sig_name_bytes = unsafe {
        from_raw_parts(
            &firmwaretableprovidersignature as *const u32 as *const u8,
            4,
        )
    }
    .to_vec();
    sig_name_bytes.reverse();
    String::from_utf8(sig_name_bytes).unwrap_or_else(|e| format!("Error({})", e))
}
