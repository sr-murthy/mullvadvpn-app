#![allow(clippy::undocumented_unsafe_blocks)] // Remove me if you dare.

use once_cell::sync::OnceCell;
use std::{ffi::CStr, fmt, io, mem, os::windows::io::RawHandle, path::Path, ptr};
use talpid_types::{ErrorExt, win32_err};
use widestring::{U16CStr, U16CString};
use windows_sys::{
    Win32::{
        Foundation::{FreeLibrary, HMODULE},
        NetworkManagement::{IpHelper::ConvertInterfaceLuidToGuid, Ndis::NET_LUID_LH},
        System::{
            Com::StringFromGUID2,
            LibraryLoader::{GetProcAddress, LOAD_WITH_ALTERED_SEARCH_PATH, LoadLibraryExW},
            Registry::REG_SAM_FLAGS,
        },
    },
    core::GUID,
};
use winreg::{
    RegKey,
    enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE},
};

/// Shared `WintunDll` instance
static WINTUN_DLL: OnceCell<WintunDll> = OnceCell::new();

type WintunCreateAdapterFn = unsafe extern "stdcall" fn(
    name: *const u16,
    tunnel_type: *const u16,
    requested_guid: *const GUID,
) -> RawHandle;

type WintunOpenAdapterFn = unsafe extern "stdcall" fn(name: *const u16) -> RawHandle;

type WintunCloseAdapterFn = unsafe extern "stdcall" fn(adapter: RawHandle);

type WintunGetAdapterLuidFn =
    unsafe extern "stdcall" fn(adapter: RawHandle, luid: *mut NET_LUID_LH);

type WintunLoggerCbFn = extern "stdcall" fn(WintunLoggerLevel, u64, *const u16);

type WintunSetLoggerFn = unsafe extern "stdcall" fn(Option<WintunLoggerCbFn>);

#[repr(C)]
#[allow(dead_code)]
enum WintunLoggerLevel {
    Info,
    Warn,
    Err,
}

pub struct WintunDll {
    handle: HMODULE,
    func_create: WintunCreateAdapterFn,
    func_open: WintunOpenAdapterFn,
    func_close: WintunCloseAdapterFn,
    func_get_adapter_luid: WintunGetAdapterLuidFn,
    func_set_logger: WintunSetLoggerFn,
}

unsafe impl Send for WintunDll {}
unsafe impl Sync for WintunDll {}

/// Represents a Wintun adapter.
pub struct WintunAdapter {
    dll_handle: &'static WintunDll,
    handle: RawHandle,
    name: U16CString,
}

impl fmt::Debug for WintunAdapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WintunAdapter")
            .field("handle", &self.handle)
            .finish()
    }
}

unsafe impl Send for WintunAdapter {}
unsafe impl Sync for WintunAdapter {}

impl WintunAdapter {
    pub fn create(
        dll_handle: &'static WintunDll,
        name: &U16CStr,
        tunnel_type: &U16CStr,
        requested_guid: Option<GUID>,
    ) -> io::Result<Self> {
        let handle = dll_handle.create_adapter(name, tunnel_type, requested_guid)?;
        let adapter = Self {
            dll_handle,
            handle,
            name: name.to_owned(),
        };
        adapter.restore_missing_component_id();
        Ok(adapter)
    }

    pub fn prepare_interface(&self) {
        if let Err(error) =
            talpid_tunnel::network_interface::initialize_interfaces(self.luid(), None, None)
        {
            log::error!(
                "{}",
                error.display_chain_with_msg("Failed to set tunnel interface metric"),
            );
        }
    }

    pub fn name(&self) -> U16CString {
        self.name.clone()
    }

    pub fn luid(&self) -> NET_LUID_LH {
        unsafe { self.dll_handle.get_adapter_luid(self.handle) }
    }

    pub fn guid(&self) -> io::Result<GUID> {
        let mut guid = mem::MaybeUninit::zeroed();
        win32_err!(unsafe { ConvertInterfaceLuidToGuid(&self.luid(), guid.as_mut_ptr()) })?;
        Ok(unsafe { guid.assume_init() })
    }

    fn restore_missing_component_id(&self) {
        let assigned_guid = match self.guid() {
            Ok(guid) => guid,
            Err(error) => {
                log::error!(
                    "{}",
                    error.display_chain_with_msg("Cannot identify adapter guid")
                );
                return;
            }
        };
        let assigned_guid_string = string_from_guid(&assigned_guid);

        // Workaround: OpenVPN looks up "ComponentId" to identify tunnel devices.
        // If Wintun fails to create this registry value, create it here.
        let adapter_key = find_adapter_registry_key(&assigned_guid_string, KEY_READ | KEY_WRITE);
        match adapter_key {
            Ok(adapter_key) => {
                let component_id: io::Result<String> = adapter_key.get_value("ComponentId");
                match component_id {
                    Ok(_) => (),
                    Err(error) => {
                        if error.kind() == io::ErrorKind::NotFound {
                            if let Err(error) = adapter_key.set_value("ComponentId", &"wintun") {
                                log::error!(
                                    "{}",
                                    error.display_chain_with_msg(
                                        "Failed to set ComponentId registry value"
                                    )
                                );
                            }
                        }
                    }
                }
            }
            Err(error) => {
                log::error!(
                    "{}",
                    error.display_chain_with_msg("Failed to find network adapter registry key")
                );
            }
        }
    }
}

impl Drop for WintunAdapter {
    fn drop(&mut self) {
        unsafe { self.dll_handle.close_adapter(self.handle) };
    }
}

impl WintunDll {
    pub fn instance(resource_dir: &Path) -> io::Result<&'static Self> {
        WINTUN_DLL.get_or_try_init(|| Self::new(resource_dir))
    }

    fn new(resource_dir: &Path) -> io::Result<Self> {
        let wintun_dll = U16CString::from_os_str_truncate(resource_dir.join("wintun.dll"));

        let handle =
            unsafe { LoadLibraryExW(wintun_dll.as_ptr(), 0, LOAD_WITH_ALTERED_SEARCH_PATH) };
        if handle == 0 {
            return Err(io::Error::last_os_error());
        }
        Self::new_inner(handle, Self::get_proc_address)
    }

    fn new_inner(
        handle: HMODULE,
        get_proc_fn: unsafe fn(HMODULE, &CStr) -> io::Result<unsafe extern "system" fn() -> isize>,
    ) -> io::Result<Self> {
        Ok(WintunDll {
            handle,
            func_create: unsafe {
                *((&get_proc_fn(handle, c"WintunCreateAdapter")?) as *const _ as *const _)
            },
            func_open: unsafe {
                *((&get_proc_fn(handle, c"WintunOpenAdapter")?) as *const _ as *const _)
            },
            func_close: unsafe {
                *((&get_proc_fn(handle, c"WintunCloseAdapter")?) as *const _ as *const _)
            },
            func_get_adapter_luid: unsafe {
                *((&get_proc_fn(handle, c"WintunGetAdapterLUID")?) as *const _ as *const _)
            },
            func_set_logger: unsafe {
                *((&get_proc_fn(handle, c"WintunSetLogger")?) as *const _ as *const _)
            },
        })
    }

    unsafe fn get_proc_address(
        handle: HMODULE,
        name: &CStr,
    ) -> io::Result<unsafe extern "system" fn() -> isize> {
        let handle = unsafe { GetProcAddress(handle, name.as_ptr() as *const u8) };
        handle.ok_or(io::Error::last_os_error())
    }

    pub fn create_adapter(
        &self,
        name: &U16CStr,
        tunnel_type: &U16CStr,
        requested_guid: Option<GUID>,
    ) -> io::Result<RawHandle> {
        let guid_ptr = match requested_guid.as_ref() {
            Some(guid) => guid as *const _,
            None => ptr::null_mut(),
        };
        let handle = unsafe { (self.func_create)(name.as_ptr(), tunnel_type.as_ptr(), guid_ptr) };
        if handle.is_null() {
            log::error!(
                "Failed to create Wintun adapter: {}",
                io::Error::last_os_error()
            );
            // This is an attempt to fix the elusive "Failed to create Wintun adapter" error.
            // we cannot reproduce the issue on our end, but if it is caused by an existing adapter
            // that hasn't been cleaned up properly, it may help to open the adapter and return it.
            log::info!(
                "Attempting to open existing adapter with name: '{}'",
                name.to_string_lossy()
            );
            let handle = unsafe { (self.func_open)(name.as_ptr()) };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            } else {
                return Ok(handle);
            }
        }
        Ok(handle)
    }

    pub unsafe fn close_adapter(&self, adapter: RawHandle) {
        unsafe { (self.func_close)(adapter) };
    }

    pub unsafe fn get_adapter_luid(&self, adapter: RawHandle) -> NET_LUID_LH {
        let mut luid = mem::MaybeUninit::<NET_LUID_LH>::zeroed();
        unsafe {
            (self.func_get_adapter_luid)(adapter, luid.as_mut_ptr());
            luid.assume_init()
        }
    }

    pub fn activate_logging(&'static self) -> WintunLoggerHandle {
        WintunLoggerHandle::from_handle(self)
    }

    fn set_logger(&self, logger: Option<WintunLoggerCbFn>) {
        unsafe { (self.func_set_logger)(logger) };
    }
}

impl Drop for WintunDll {
    fn drop(&mut self) {
        unsafe { FreeLibrary(self.handle) };
    }
}

pub struct WintunLoggerHandle {
    dll_handle: &'static WintunDll,
}

impl WintunLoggerHandle {
    fn from_handle(dll_handle: &'static WintunDll) -> Self {
        dll_handle.set_logger(Some(Self::callback));
        Self { dll_handle }
    }

    extern "stdcall" fn callback(level: WintunLoggerLevel, _timestamp: u64, message: *const u16) {
        if message.is_null() {
            return;
        }
        let message = unsafe { U16CStr::from_ptr_str(message) };

        use WintunLoggerLevel::*;

        match level {
            Info => log::info!("[Wintun] {}", message.to_string_lossy()),
            Warn => log::warn!("[Wintun] {}", message.to_string_lossy()),
            Err => log::error!("[Wintun] {}", message.to_string_lossy()),
        }
    }
}

impl fmt::Debug for WintunLoggerHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WintunLogger").finish()
    }
}

impl Drop for WintunLoggerHandle {
    fn drop(&mut self) {
        self.dll_handle.set_logger(None);
    }
}

/// Returns the registry key for a network device identified by its GUID.
fn find_adapter_registry_key(find_guid: &str, permissions: REG_SAM_FLAGS) -> io::Result<RegKey> {
    let net_devs = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey_with_flags(
        r"SYSTEM\CurrentControlSet\Control\Class\{4d36e972-e325-11ce-bfc1-08002be10318}",
        permissions,
    )?;
    let find_guid = find_guid.to_lowercase();

    for subkey_name in net_devs.enum_keys() {
        let subkey_name = match subkey_name {
            Ok(subkey_name) => subkey_name,
            Err(_error) => continue,
        };

        let subkey: io::Result<RegKey> = net_devs.open_subkey_with_flags(&subkey_name, permissions);
        if let Ok(subkey) = subkey {
            let guid_str: io::Result<String> = subkey.get_value("NetCfgInstanceId");
            if let Ok(guid_str) = guid_str {
                if guid_str.to_lowercase() == find_guid {
                    return Ok(subkey);
                }
            }
        }
    }

    Err(io::Error::new(io::ErrorKind::NotFound, "device not found"))
}

/// Obtain a string representation for a GUID object.
fn string_from_guid(guid: &GUID) -> String {
    let mut buffer = [0u16; 40];

    // SAFETY: `guid` and `buffer` are valid references.
    let length =
        unsafe { StringFromGUID2(guid, buffer.as_mut_ptr(), buffer.len() as i32 - 1) } as usize;

    // cannot fail because `buffer` is large enough
    assert!(length > 0);
    let length = length - 1;
    String::from_utf16(&buffer[0..length]).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get_proc_fn(
        _handle: HMODULE,
        _symbol: &CStr,
    ) -> io::Result<unsafe extern "system" fn() -> isize> {
        Ok(null_fn)
    }

    #[test]
    fn test_wintun_imports() {
        WintunDll::new_inner(0, get_proc_fn).unwrap();
    }

    #[test]
    fn guid_to_string() {
        let guids = [
            (
                "{AFE43773-E1F8-4EBB-8536-576AB86AFE9A}",
                GUID {
                    data1: 0xAFE43773,
                    data2: 0xE1F8,
                    data3: 0x4EBB,
                    data4: [0x85, 0x36, 0x57, 0x6A, 0xB8, 0x6A, 0xFE, 0x9A],
                },
            ),
            (
                "{00000000-0000-0000-0000-000000000000}",
                GUID {
                    data1: 0,
                    data2: 0,
                    data3: 0,
                    data4: [0; 8],
                },
            ),
        ];

        for (expected_str, guid) in &guids {
            assert_eq!(
                string_from_guid(guid).as_str().to_lowercase(),
                expected_str.to_lowercase()
            );
        }
    }

    unsafe extern "system" fn null_fn() -> isize {
        unreachable!("unexpected call of function")
    }
}
