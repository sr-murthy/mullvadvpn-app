#![allow(clippy::undocumented_unsafe_blocks)] // Remove me if you dare.

use socket2::SockAddr;
use std::{
    ffi::{OsStr, OsString},
    fmt, io,
    mem::{self, MaybeUninit},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6},
    os::windows::ffi::{OsStrExt, OsStringExt},
    sync::Mutex,
    time::{Duration, Instant},
};
use talpid_types::win32_err;
use windows_sys::{
    Win32::{
        Foundation::{ERROR_NOT_FOUND, HANDLE},
        NetworkManagement::{
            IpHelper::{
                CancelMibChangeNotify2, ConvertInterfaceAliasToLuid, ConvertInterfaceLuidToAlias,
                ConvertInterfaceLuidToGuid, ConvertInterfaceLuidToIndex,
                CreateUnicastIpAddressEntry, FreeMibTable, GetIpInterfaceEntry,
                GetUnicastIpAddressEntry, GetUnicastIpAddressTable,
                InitializeUnicastIpAddressEntry, MIB_IPINTERFACE_ROW, MIB_UNICASTIPADDRESS_ROW,
                MIB_UNICASTIPADDRESS_TABLE, MibAddInstance, NotifyIpInterfaceChange,
                SetIpInterfaceEntry,
            },
            Ndis::{IF_MAX_STRING_SIZE, NET_LUID_LH},
        },
        Networking::WinSock::{
            AF_INET, AF_INET6, AF_UNSPEC, IN_ADDR, IN6_ADDR, IpDadStateDeprecated,
            IpDadStateDuplicate, IpDadStateInvalid, IpDadStatePreferred, IpDadStateTentative,
            NL_DAD_STATE, SOCKADDR_IN as sockaddr_in, SOCKADDR_IN6 as sockaddr_in6, SOCKADDR_INET,
            SOCKADDR_STORAGE as sockaddr_storage,
        },
    },
    core::GUID,
};

/// Result type for this module.
pub type Result<T> = std::result::Result<T, Error>;

const DAD_CHECK_TIMEOUT: Duration = Duration::from_secs(5);
const DAD_CHECK_INTERVAL: Duration = Duration::from_millis(100);

/// Errors returned by some functions in this module.
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// Error returned from `ConvertInterfaceAliasToLuid`
    #[cfg(windows)]
    #[error("Cannot find LUID for virtual adapter")]
    NoDeviceLuid(#[source] io::Error),

    /// Error returned from `GetUnicastIpAddressTable`/`GetUnicastIpAddressEntry`
    #[cfg(windows)]
    #[error("Failed to obtain unicast IP address table")]
    ObtainUnicastAddress(#[source] io::Error),

    /// `GetUnicastIpAddressTable` contained no addresses for the interface
    #[cfg(windows)]
    #[error("Found no addresses for the given adapter")]
    NoUnicastAddress,

    /// Error returned from `CreateUnicastIpAddressEntry`
    #[cfg(windows)]
    #[error("Failed to create unicast IP address")]
    CreateUnicastEntry(#[source] io::Error),

    /// Unexpected DAD state returned for a unicast address
    #[cfg(windows)]
    #[error("Unexpected DAD state")]
    DadStateError(#[source] DadStateError),

    /// DAD check failed.
    #[cfg(windows)]
    #[error("Timed out waiting on tunnel device")]
    DeviceReadyTimeout,

    /// Unicast DAD check fail.
    #[cfg(windows)]
    #[error("Unicast channel sender was unexpectedly dropped")]
    UnicastSenderDropped,

    /// Unknown address family
    #[error("Unknown address family: {0}")]
    UnknownAddressFamily(u16),
}

/// Handles cases where there DAD state is neither tentative nor preferred.
#[derive(thiserror::Error, Debug)]
pub enum DadStateError {
    /// Invalid DAD state.
    #[error("Invalid DAD state")]
    Invalid,

    /// Duplicate unicast address.
    #[error("A duplicate IP address was detected")]
    Duplicate,

    /// Deprecated unicast address.
    #[error("The IP address has been deprecated")]
    Deprecated,

    /// Unknown DAD state constant.
    #[error("Unknown DAD state: {0}")]
    Unknown(i32),
}

#[allow(non_upper_case_globals)]
impl From<NL_DAD_STATE> for DadStateError {
    fn from(state: NL_DAD_STATE) -> DadStateError {
        match state {
            IpDadStateInvalid => DadStateError::Invalid,
            IpDadStateDuplicate => DadStateError::Duplicate,
            IpDadStateDeprecated => DadStateError::Deprecated,
            other => DadStateError::Unknown(other),
        }
    }
}

impl AddressFamily {
    /// Convert one of the `AF_*` constants to an [`AddressFamily`].
    pub fn try_from_af_family(family: u16) -> Result<AddressFamily> {
        match family {
            AF_INET => Ok(AddressFamily::Ipv4),
            AF_INET6 => Ok(AddressFamily::Ipv6),
            family => Err(Error::UnknownAddressFamily(family)),
        }
    }

    /// Convert an [`AddressFamily`] to one of the `AF_*` constants.
    pub fn to_af_family(&self) -> u16 {
        match self {
            Self::Ipv4 => AF_INET,
            Self::Ipv6 => AF_INET6,
        }
    }
}

/// Context for [`notify_ip_interface_change`]. When it is dropped,
/// the callback is unregistered.
pub struct IpNotifierHandle<'a> {
    #[allow(clippy::type_complexity)]
    callback: Mutex<Box<dyn FnMut(&MIB_IPINTERFACE_ROW, i32) + Send + 'a>>,
    handle: HANDLE,
}

unsafe impl Send for IpNotifierHandle<'_> {}

impl Drop for IpNotifierHandle<'_> {
    fn drop(&mut self) {
        unsafe { CancelMibChangeNotify2(self.handle) };
    }
}

unsafe extern "system" fn inner_callback(
    context: *const std::ffi::c_void,
    row: *const MIB_IPINTERFACE_ROW,
    notify_type: i32,
) {
    unsafe {
        let context = &mut *(context as *mut IpNotifierHandle<'_>);
        context
            .callback
            .lock()
            .expect("NotifyIpInterfaceChange mutex poisoned")(&*row, notify_type);
    }
}

/// Registers a callback function that is invoked when an interface is added, removed,
/// or changed.
pub fn notify_ip_interface_change<'a, T: FnMut(&MIB_IPINTERFACE_ROW, i32) + Send + 'a>(
    callback: T,
    family: Option<AddressFamily>,
) -> io::Result<Box<IpNotifierHandle<'a>>> {
    let mut context = Box::new(IpNotifierHandle {
        callback: Mutex::new(Box::new(callback)),
        handle: 0,
    });

    win32_err!(unsafe {
        NotifyIpInterfaceChange(
            af_family_from_family(family),
            Some(inner_callback),
            &mut *context as *mut _ as *mut _,
            0,
            (&mut context.handle) as *mut _,
        )
    })?;
    Ok(context)
}

/// Returns information about a network IP interface.
pub fn get_ip_interface_entry(
    family: AddressFamily,
    luid: &NET_LUID_LH,
) -> io::Result<MIB_IPINTERFACE_ROW> {
    let mut row: MIB_IPINTERFACE_ROW = unsafe { mem::zeroed() };
    row.Family = family as u16;
    row.InterfaceLuid = *luid;

    win32_err!(unsafe { GetIpInterfaceEntry(&mut row) })?;
    Ok(row)
}

/// Set the properties of an IP interface.
pub fn set_ip_interface_entry(row: &mut MIB_IPINTERFACE_ROW) -> io::Result<()> {
    win32_err!(unsafe { SetIpInterfaceEntry(row as *mut _) })
}

fn ip_interface_entry_exists(family: AddressFamily, luid: &NET_LUID_LH) -> io::Result<bool> {
    match get_ip_interface_entry(family, luid) {
        Ok(_) => Ok(true),
        Err(error) if error.raw_os_error() == Some(ERROR_NOT_FOUND as i32) => Ok(false),
        Err(error) => Err(error),
    }
}

/// Waits until the specified IP interfaces have attached to a given network interface.
pub async fn wait_for_interfaces(luid: NET_LUID_LH, ipv4: bool, ipv6: bool) -> io::Result<()> {
    let (tx, rx) = futures::channel::oneshot::channel();

    let mut found_ipv4 = !ipv4;
    let mut found_ipv6 = !ipv6;

    let mut tx = Some(tx);

    let _handle = notify_ip_interface_change(
        move |row, notification_type| {
            if found_ipv4 && found_ipv6 {
                return;
            }
            if notification_type != MibAddInstance {
                return;
            }
            if unsafe { row.InterfaceLuid.Value != luid.Value } {
                return;
            }
            match row.Family {
                AF_INET => found_ipv4 = true,
                AF_INET6 => found_ipv6 = true,
                _ => (),
            }
            if found_ipv4 && found_ipv6 {
                if let Some(tx) = tx.take() {
                    let _ = tx.send(());
                }
            }
        },
        None,
    )?;

    // Make sure they don't already exist
    if (!ipv4 || ip_interface_entry_exists(AddressFamily::Ipv4, &luid)?)
        && (!ipv6 || ip_interface_entry_exists(AddressFamily::Ipv6, &luid)?)
    {
        return Ok(());
    }

    let _ = rx.await;
    Ok(())
}

/// Wait for addresses to be usable on an network adapter.
pub async fn wait_for_addresses(luid: NET_LUID_LH) -> Result<()> {
    // Obtain unicast IP addresses
    let mut unicast_rows: Vec<MIB_UNICASTIPADDRESS_ROW> = get_unicast_table(None)
        .map_err(Error::ObtainUnicastAddress)?
        .into_iter()
        .filter(|row| unsafe { row.InterfaceLuid.Value == luid.Value })
        .collect();
    if unicast_rows.is_empty() {
        return Err(Error::NoUnicastAddress);
    }

    let (tx, rx) = futures::channel::oneshot::channel();
    let mut addr_check_thread = move || {
        // Poll DAD status using GetUnicastIpAddressEntry
        // https://docs.microsoft.com/en-us/windows/win32/api/netioapi/nf-netioapi-createunicastipaddressentry

        let deadline = Instant::now() + DAD_CHECK_TIMEOUT;
        while Instant::now() < deadline {
            let mut ready = true;

            for row in &mut unicast_rows {
                win32_err!(unsafe { GetUnicastIpAddressEntry(row) })
                    .map_err(Error::ObtainUnicastAddress)?;
                if row.DadState == IpDadStateTentative {
                    ready = false;
                    break;
                }
                if row.DadState != IpDadStatePreferred {
                    return Err(Error::DadStateError(DadStateError::from(row.DadState)));
                }
            }

            if ready {
                return Ok(());
            }
            std::thread::sleep(DAD_CHECK_INTERVAL);
        }

        Err(Error::DeviceReadyTimeout)
    };
    std::thread::spawn(move || {
        let _ = tx.send(addr_check_thread());
    });
    rx.await.map_err(|_| Error::UnicastSenderDropped)?
}

/// Returns the first unicast IP address for the given interface.
pub fn get_ip_address_for_interface(
    family: AddressFamily,
    luid: NET_LUID_LH,
) -> Result<Option<IpAddr>> {
    match get_unicast_table(Some(family))
        .map_err(Error::ObtainUnicastAddress)?
        .into_iter()
        .find(|row| unsafe { row.InterfaceLuid.Value == luid.Value })
    {
        Some(row) => Ok(Some(try_socketaddr_from_inet_sockaddr(row.Address)?.ip())),
        None => Ok(None),
    }
}

/// Adds a unicast IP address for the given interface.
pub fn add_ip_address_for_interface(luid: NET_LUID_LH, address: IpAddr) -> Result<()> {
    let mut row = unsafe { mem::zeroed() };
    unsafe { InitializeUnicastIpAddressEntry(&mut row) };

    row.InterfaceLuid = luid;
    row.Address = inet_sockaddr_from_socketaddr(SocketAddr::new(address, 0));
    row.DadState = IpDadStatePreferred;
    row.OnLinkPrefixLength = 255;

    win32_err!(unsafe { CreateUnicastIpAddressEntry(&row) }).map_err(Error::CreateUnicastEntry)
}

/// Sets MTU on the specified network interface identified by `luid`.
pub fn set_mtu(mtu: u32, luid: NET_LUID_LH, ip_family: AddressFamily) -> io::Result<()> {
    let mut row = get_ip_interface_entry(ip_family, &luid)?;

    row.NlMtu = mtu;

    set_ip_interface_entry(&mut row)
}

/// Returns the unicast IP address table. If `family` is `None`, then addresses for all families are
/// returned.
pub fn get_unicast_table(
    family: Option<AddressFamily>,
) -> io::Result<Vec<MIB_UNICASTIPADDRESS_ROW>> {
    let mut unicast_rows = vec![];
    let mut unicast_table: *mut MIB_UNICASTIPADDRESS_TABLE = std::ptr::null_mut();

    win32_err!(unsafe {
        GetUnicastIpAddressTable(af_family_from_family(family), &mut unicast_table)
    })?;
    let first_row = unsafe { &(*unicast_table).Table[0] } as *const MIB_UNICASTIPADDRESS_ROW;
    for i in 0..unsafe { *unicast_table }.NumEntries {
        unicast_rows.push(unsafe { *(first_row.offset(i as isize)) });
    }
    unsafe { FreeMibTable(unicast_table as *const _) };

    Ok(unicast_rows)
}

/// Returns the index of a network interface given its LUID.
pub fn index_from_luid(luid: &NET_LUID_LH) -> io::Result<u32> {
    let mut index = 0u32;
    win32_err!(unsafe { ConvertInterfaceLuidToIndex(luid, &mut index) })?;
    Ok(index)
}

/// Returns the GUID of a network interface given its LUID.
pub fn guid_from_luid(luid: &NET_LUID_LH) -> io::Result<GUID> {
    let mut guid = MaybeUninit::zeroed();
    win32_err!(unsafe { ConvertInterfaceLuidToGuid(luid, guid.as_mut_ptr()) })?;
    Ok(unsafe { guid.assume_init() })
}

/// Returns the LUID of an interface given its alias.
pub fn luid_from_alias<T: AsRef<OsStr>>(alias: T) -> io::Result<NET_LUID_LH> {
    let alias_wide: Vec<u16> = alias
        .as_ref()
        .encode_wide()
        .chain(std::iter::once(0u16))
        .collect();
    let mut luid: NET_LUID_LH = unsafe { std::mem::zeroed() };
    win32_err!(unsafe { ConvertInterfaceAliasToLuid(alias_wide.as_ptr(), &mut luid) })?;
    Ok(luid)
}

/// Returns the alias of an interface given its LUID.
pub fn alias_from_luid(luid: &NET_LUID_LH) -> io::Result<OsString> {
    let mut buffer = [0u16; IF_MAX_STRING_SIZE as usize + 1];
    win32_err!(unsafe { ConvertInterfaceLuidToAlias(luid, buffer.as_mut_ptr(), buffer.len()) })?;
    let nul = buffer.iter().position(|&c| c == 0u16).unwrap();
    Ok(OsString::from_wide(&buffer[0..nul]))
}

fn af_family_from_family(family: Option<AddressFamily>) -> u16 {
    family.map(|family| family as u16).unwrap_or(AF_UNSPEC)
}

/// Converts an `Ipv4Addr` to `IN_ADDR`
pub fn inaddr_from_ipaddr(addr: Ipv4Addr) -> IN_ADDR {
    let sockaddr = SockAddr::from(SocketAddr::V4(SocketAddrV4::new(addr, 0)));
    unsafe { *(sockaddr.as_ptr() as *const sockaddr_in) }.sin_addr
}

/// Converts an `Ipv6Addr` to `IN6_ADDR`
pub fn in6addr_from_ipaddr(addr: Ipv6Addr) -> IN6_ADDR {
    let sockaddr = SockAddr::from(SocketAddr::V6(SocketAddrV6::new(addr, 0, 0, 0)));
    unsafe { *(sockaddr.as_ptr() as *const sockaddr_in6) }.sin6_addr
}

/// Converts an `IN_ADDR` to `Ipv4Addr`
pub fn ipaddr_from_inaddr(addr: IN_ADDR) -> Ipv4Addr {
    Ipv4Addr::from(unsafe { addr.S_un.S_addr }.to_ne_bytes())
}

/// Converts an `IN6_ADDR` to `Ipv6Addr`
pub fn ipaddr_from_in6addr(addr: IN6_ADDR) -> Ipv6Addr {
    Ipv6Addr::from(unsafe { addr.u.Byte })
}

/// Converts a `SocketAddr` to `SOCKADDR_INET`
pub fn inet_sockaddr_from_socketaddr(addr: SocketAddr) -> SOCKADDR_INET {
    // SAFETY: SOCKADDR_INET is a union of C structs, these can be safely zeroed.
    let mut sockaddr: SOCKADDR_INET = unsafe { mem::zeroed() };
    match addr {
        // SAFETY: `*const sockaddr` may be treated as `*const sockaddr_in` since we know it's a v4
        // address.
        SocketAddr::V4(_) => unsafe {
            sockaddr.Ipv4 = *(SockAddr::from(addr).as_ptr() as *const _)
        },
        // SAFETY: `*const sockaddr` may be treated as `*const sockaddr_in6` since we know it's a v6
        // address.
        SocketAddr::V6(_) => unsafe {
            sockaddr.Ipv6 = *(SockAddr::from(addr).as_ptr() as *const _)
        },
    }
    sockaddr
}

/// Converts a `SOCKADDR_INET` to `SocketAddr`. Returns an error if the address family is invalid.
pub fn try_socketaddr_from_inet_sockaddr(addr: SOCKADDR_INET) -> Result<SocketAddr> {
    // SAFETY: si_family is always valid
    let family = unsafe { addr.si_family };
    unsafe {
        let mut storage: sockaddr_storage = mem::zeroed();
        *(&mut storage as *mut _ as *mut SOCKADDR_INET) = addr;
        SockAddr::new(storage, mem::size_of_val(&addr) as i32)
    }
    .as_socket()
    .ok_or(Error::UnknownAddressFamily(family))
}

/// Address family. These correspond to the `AF_*` constants.
#[derive(Debug, Clone, Copy)]
pub enum AddressFamily {
    /// IPv4 address family
    Ipv4 = AF_INET as isize,
    /// IPv6 address family
    Ipv6 = AF_INET6 as isize,
}

impl fmt::Display for AddressFamily {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            AddressFamily::Ipv4 => write!(f, "IPv4 (AF_INET)"),
            AddressFamily::Ipv6 => write!(f, "IPv6 (AF_INET6)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sockaddr_v4() {
        let addr_v4 = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(1, 2, 3, 4), 1234));
        assert_eq!(
            addr_v4,
            try_socketaddr_from_inet_sockaddr(inet_sockaddr_from_socketaddr(addr_v4)).unwrap()
        );
    }

    #[test]
    fn test_sockaddr_v6() {
        let addr_v6 = SocketAddr::V6(SocketAddrV6::new(
            Ipv6Addr::new(1, 2, 3, 4, 5, 6, 7, 8),
            1234,
            0xa,
            0xb,
        ));
        assert_eq!(
            addr_v6,
            try_socketaddr_from_inet_sockaddr(inet_sockaddr_from_socketaddr(addr_v6)).unwrap()
        );
    }
}
