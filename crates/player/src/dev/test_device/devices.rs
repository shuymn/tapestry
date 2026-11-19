//! `tapestry test-device devices` and the discovery the other commands
//! share (docs/test-device.md "Host Commands"): the composite parent
//! `USB\VID_1209&PID_0001` through the configuration manager, its keyboard
//! and pad interfaces with their HID collections' usages, the Raw Input
//! handles of each by interface path and container id with their HID
//! capabilities, the link's COM port, and the active configuration
//! descriptor (each endpoint's `bInterval`) read from the parent's hub
//! port. The device is bound to its USB location path
//! (`DEVPKEY_Device_LocationPaths` of the composite parent, [`port_path`]),
//! and every step that reaches it refuses a choice among several devices
//! ([`pick`]); the boot ROM's drive is tied to a location through its
//! volume's disk and the disk's ancestors in the configuration manager's
//! tree ([`volume_location`]). `devices` checks the device's shape
//! ([`shape`]) and fails naming the first broken part.

use std::ffi::c_void;
use std::io::Write as _;
use std::path::Path;

use anyhow::{Context as _, bail};
use serde_json::{Value, json};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_GETIDLIST_FILTER_ENUMERATOR,
    CM_GETIDLIST_FILTER_PRESENT, CM_Get_Child, CM_Get_DevNode_PropertyW,
    CM_Get_Device_ID_List_SizeW, CM_Get_Device_ID_ListW, CM_Get_Device_IDW,
    CM_Get_Device_Interface_List_SizeW, CM_Get_Device_Interface_ListW,
    CM_Get_Device_Interface_PropertyW, CM_Get_Parent, CM_Get_Sibling, CM_LOCATE_DEVNODE_NORMAL,
    CM_Locate_DevNodeW, CM_Open_DevNode_Key, CM_REGISTRY_HARDWARE, CR_SUCCESS,
    RegDisposition_OpenExisting,
};
use windows::Win32::Devices::HumanInterfaceDevice::{
    HIDP_BUTTON_CAPS, HIDP_CAPS, HIDP_STATUS_SUCCESS, HIDP_VALUE_CAPS, HidP_GetButtonCaps,
    HidP_GetCaps, HidP_GetValueCaps, HidP_Input, PHIDP_PREPARSED_DATA,
};
use windows::Win32::Devices::Properties::{
    DEVPKEY_Device_Address, DEVPKEY_Device_ContainerId, DEVPKEY_Device_HardwareIds,
    DEVPKEY_Device_InstanceId, DEVPKEY_Device_LocationInfo, DEVPKEY_Device_LocationPaths,
    DEVPROPTYPE,
};
use windows::Win32::Devices::Usb::{
    GUID_DEVINTERFACE_USB_HUB, IOCTL_USB_GET_DESCRIPTOR_FROM_NODE_CONNECTION,
};
use windows::Win32::Foundation::{DEVPROPKEY, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Registry::{HKEY, KEY_READ, RegCloseKey, RegQueryValueExW};
use windows::Win32::UI::Input::{
    GetRawInputDeviceInfoW, GetRawInputDeviceList, RAWINPUTDEVICELIST, RIDI_DEVICENAME,
    RIDI_PREPARSEDDATA, RIM_TYPEHID, RIM_TYPEKEYBOARD,
};
use windows::core::{GUID, PCWSTR, w};

use super::link::{ComPort, Connection, Endpoints, Facts, Link, Port};
use super::win::{Owned, multi_sz, qpc, until_nul, wide};

/// The parent's instance id prefix.
pub fn parent_prefix() -> String {
    format!(
        r"USB\VID_{:04X}&PID_{:04X}\",
        test_device::VID,
        test_device::PID
    )
}

/// The RP2350 boot ROM's USB vendor and product ids (RP2350 datasheet, the
/// boot ROM's USB device: VID 0x2E8A, PID 0x000F unless white-labelled).
const BOOT_ROM_VID_PID: (u16, u16) = (0x2E8A, 0x000F);

/// The boot ROM's USB device's instance id prefix.
pub fn boot_rom_prefix() -> String {
    let (vid, pid) = BOOT_ROM_VID_PID;
    format!(r"USB\VID_{vid:04X}&PID_{pid:04X}\")
}

/// Why no single device on the expected port was chosen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Several candidates are present, with these port paths.
    Several(Vec<Option<String>>),
    /// The one candidate's port path cannot be read.
    NoPort,
    /// The one candidate is on another port than the bound one.
    OtherPort { found: String, bound: String },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Several(ports) => {
                let ports: Vec<_> = ports
                    .iter()
                    .map(|p| p.as_deref().unwrap_or("(unknown port)"))
                    .collect();
                write!(
                    f,
                    "{} are present (on {}): disconnect all but one",
                    ports.len(),
                    ports.join(", ")
                )
            }
            Self::NoPort => write!(f, "its port path cannot be read"),
            Self::OtherPort { found, bound } => {
                write!(f, "it is on port {found}, not on the bound port {bound}")
            }
        }
    }
}

impl std::error::Error for Refusal {}

/// The one candidate, with its port path: none when there are none; refused
/// when there are several, when its port path is unknown, or when `bound`
/// names another port.
///
/// # Errors
///
/// The [`Refusal`].
pub fn pick<T>(
    candidates: Vec<(Option<String>, T)>,
    bound: Option<&str>,
) -> Result<Option<(String, T)>, Refusal> {
    if candidates.len() > 1 {
        return Err(Refusal::Several(
            candidates.into_iter().map(|(p, _)| p).collect(),
        ));
    }
    let Some((port, item)) = candidates.into_iter().next() else {
        return Ok(None);
    };
    let port = port.ok_or(Refusal::NoPort)?;
    match bound {
        Some(b) if b != port => Err(Refusal::OtherPort {
            found: port,
            bound: b.to_owned(),
        }),
        _ => Ok(Some((port, item))),
    }
}

/// The location to compare: the first of `DEVPKEY_Device_LocationPaths`
/// (`PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(3)`), else the hub's instance id
/// and the location information (`Port_#0003.Hub_#0001`).
pub fn port_path_of(
    location_paths: &[String],
    hub_and_location: Option<(&str, &str)>,
) -> Option<String> {
    location_paths
        .first()
        .filter(|p| !p.is_empty())
        .cloned()
        .or_else(|| hub_and_location.map(|(hub, loc)| format!("{hub}|{loc}")))
}

/// The interface number an instance id or interface path names (`&MI_01`).
pub fn mi_of(s: &str) -> Option<u8> {
    let upper = s.to_ascii_uppercase();
    let at = upper.find("&MI_")?;
    let digits = upper.get(at + 4..at + 6)?;
    u8::from_str_radix(digits, 16).ok()
}

/// The device instance id a Raw Input interface path belongs to:
/// `\\?\HID#VID_1209&PID_0001&MI_01#7&1&0&0000#{guid}` is
/// `HID\VID_1209&PID_0001&MI_01\7&1&0&0000`.
pub fn instance_of_path(path: &str) -> String {
    let body = path.strip_prefix(r"\\?\").unwrap_or(path);
    let body = body
        .rfind("#{")
        .map_or(body, |i| body.get(..i).unwrap_or(body));
    body.replace('#', "\\")
}

/// The usage page and usage a HID function's hardware ids name
/// (`HID_DEVICE_UP:0001_U:0005`).
pub fn usage_of(hardware_ids: &[String]) -> Option<(u16, u16)> {
    hardware_ids.iter().find_map(|id| {
        let rest = id.to_ascii_uppercase();
        let rest = rest.strip_prefix("HID_DEVICE_UP:")?;
        let (page, usage) = rest.split_once("_U:")?;
        Some((
            u16::from_str_radix(page, 16).ok()?,
            u16::from_str_radix(usage, 16).ok()?,
        ))
    })
}

/// `bcdDevice` from the parent's hardware ids (`...&REV_0108`).
pub fn revision_of(hardware_ids: &[String]) -> Option<u16> {
    hardware_ids.iter().find_map(|id| {
        let upper = id.to_ascii_uppercase();
        let at = upper.find("&REV_")?;
        u16::from_str_radix(upper.get(at + 5..at + 9)?, 16).ok()
    })
}

/// One endpoint of the active configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub interface: u8,
    pub address: u8,
    pub attributes: u8,
    pub max_packet: u16,
    pub interval: u8,
}

/// The endpoints a configuration descriptor declares, each with the
/// interface it follows.
pub fn endpoints(config: &[u8]) -> Vec<Endpoint> {
    let mut out = Vec::new();
    let mut interface = 0;
    let mut at = 0;
    while let Some(&len) = config.get(at) {
        let len = usize::from(len);
        if len < 2 {
            break;
        }
        let Some(d) = config.get(at..at + len) else {
            break;
        };
        match d {
            [_, 4, number, ..] => interface = *number,
            [_, 5, address, attributes, lo, hi, interval, ..] => out.push(Endpoint {
                interface,
                address: *address,
                attributes: *attributes,
                max_packet: u16::from_le_bytes([*lo, *hi]),
                interval: *interval,
            }),
            _ => {}
        }
        at += len;
    }
    out
}

/// A HID collection's input capabilities, from its preparsed data.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HidCaps {
    pub usage_page: u16,
    pub usage: u16,
    pub input_len: u16,
    /// The button page's usage range.
    pub buttons: Option<(u16, u16)>,
    /// X's logical range.
    pub x: Option<(i32, i32)>,
}

/// A Raw Input device of the test device.
#[derive(Clone, Debug)]
pub struct RawSource {
    pub handle: usize,
    pub path: String,
    pub instance: String,
    pub keyboard: bool,
    pub mi: Option<u8>,
    pub container: Option<u128>,
    pub caps: Option<HidCaps>,
}

/// One function (interface) of the composite device.
#[derive(Clone, Debug)]
pub struct Function {
    pub mi: u8,
    pub id: String,
    /// The HID collection device under it, for interfaces 0 and 1.
    pub hid: Option<String>,
    pub usage: Option<(u16, u16)>,
}

/// The composite device as found.
#[derive(Clone, Debug)]
pub struct Found {
    pub parent: String,
    pub revision: Option<u16>,
    pub location: Option<String>,
    /// The physical port path the device is bound by ([`port_path_of`]).
    pub port_path: String,
    pub hub: Option<String>,
    pub port: Option<u32>,
    pub container: Option<u128>,
    pub functions: Vec<Function>,
    pub com: Option<String>,
    pub raw: Vec<RawSource>,
    pub config: Result<Vec<u8>, String>,
    /// The host counter when the enumeration ended.
    pub at: i64,
}

impl Found {
    pub fn to_json(&self) -> Value {
        let guid = |g: Option<u128>| g.map(|g| format!("{:032x}", g));
        let config = match &self.config {
            Ok(c) => json!({
                "bytes": hex(c),
                "endpoints": endpoints(c).iter().map(|e| json!({
                    "interface": e.interface,
                    "address": e.address,
                    "attributes": e.attributes,
                    "max_packet": e.max_packet,
                    "interval": e.interval,
                })).collect::<Vec<_>>(),
            }),
            Err(e) => json!({ "error": e }),
        };
        json!({
            "parent": self.parent,
            "revision": self.revision,
            "location": self.location,
            "port_path": self.port_path,
            "hub": self.hub,
            "port": self.port,
            "container": guid(self.container),
            "functions": self.functions.iter().map(|f| json!({
                "mi": f.mi,
                "id": f.id,
                "hid": f.hid,
                "usage_page": f.usage.map(|u| u.0),
                "usage": f.usage.map(|u| u.1),
            })).collect::<Vec<_>>(),
            "com": self.com,
            "raw": self.raw.iter().map(|r| json!({
                "handle": r.handle,
                "path": r.path,
                "instance": r.instance,
                "keyboard": r.keyboard,
                "mi": r.mi,
                "container": guid(r.container),
                "caps": r.caps.map(|c| json!({
                    "usage_page": c.usage_page,
                    "usage": c.usage,
                    "input_len": c.input_len,
                    "buttons": c.buttons.map(|b| [b.0, b.1]),
                    "x": c.x.map(|x| [x.0, x.1]),
                })),
            })).collect::<Vec<_>>(),
            "config": config,
            "q": self.at,
        })
    }
}

/// Checks the device's shape (docs/test-device.md "Verification", F1's
/// enumeration): the keyboard (Generic Desktop 0x06) on interface 0 and
/// the pad (0x05) on interface 1, each with its Raw Input handle in the
/// parent's container; the pad's input report 19 bytes (18 and the report
/// id 0), buttons 1-16 and X 0-255; the link's COM port on interface 2,
/// answering `hello` of this protocol version with `faults 0`, `quiet 1`
/// and a session token; both HID endpoints' `bInterval` matching the mode
/// byte; the parent's location.
///
/// # Errors
///
/// The first broken part, named.
pub fn shape(found: &Found, hello: Result<&Facts, &str>) -> Result<(), String> {
    let function = |mi: u8, usage: u16, name: &str| -> Result<(), String> {
        match found.functions.iter().find(|f| f.mi == mi) {
            None => Err(format!("no interface {mi} ({name})")),
            Some(f) if f.usage != Some((input::hid::DESKTOP_PAGE, usage)) => Err(format!(
                "interface {mi} ({name}) has usage {:?}, not Generic Desktop {usage:#04x}",
                f.usage
            )),
            Some(_) => Ok(()),
        }
    };
    if found.port_path.is_empty() {
        return Err("the parent has no location".to_owned());
    }
    function(test_device::KEYBOARD_INTERFACE, 0x06, "keyboard")?;
    function(test_device::PAD_INTERFACE, 0x05, "pad")?;
    if !found
        .functions
        .iter()
        .any(|f| f.mi == test_device::LINK_INTERFACE)
    {
        return Err(format!(
            "no interface {} (the link)",
            test_device::LINK_INTERFACE
        ));
    }
    let raw = |mi: u8, keyboard: bool| {
        found
            .raw
            .iter()
            .find(|r| r.mi == Some(mi) && r.keyboard == keyboard)
    };
    let keyboard = raw(test_device::KEYBOARD_INTERFACE, true)
        .ok_or("no Raw Input keyboard handle on interface 0")?;
    let pad =
        raw(test_device::PAD_INTERFACE, false).ok_or("no Raw Input HID handle on interface 1")?;
    for r in [keyboard, pad] {
        if r.container != found.container {
            return Err(format!(
                "Raw Input handle {} is in container {:?}, not the parent's {:?}",
                r.path, r.container, found.container
            ));
        }
    }
    let caps = pad
        .caps
        .ok_or("the pad's HID capabilities cannot be read")?;
    if (caps.usage_page, caps.usage) != (input::hid::DESKTOP_PAGE, 0x05) {
        return Err(format!(
            "the pad's collection is {:#06x}/{:#06x}, not Generic Desktop 0x05",
            caps.usage_page, caps.usage
        ));
    }
    let want_len = test_device::report::PAD_REPORT + 1;
    if usize::from(caps.input_len) != want_len {
        return Err(format!(
            "the pad's input report is {} bytes, not {want_len}",
            caps.input_len
        ));
    }
    let buttons = (1, u16::from(test_device::protocol::BUTTONS));
    if caps.buttons != Some(buttons) {
        return Err(format!(
            "the pad's buttons are {:?}, not {buttons:?}",
            caps.buttons
        ));
    }
    if caps.x != Some((0, 255)) {
        return Err(format!("the pad's X is {:?}, not 0-255", caps.x));
    }
    if found.com.is_none() {
        return Err("no COM port on interface 2".to_owned());
    }
    let hello = hello.map_err(|e| format!("the link's `hello`: {e}"))?;
    if hello.faults {
        return Err(format!("the build {} has debug faults", hello.build));
    }
    if !hello.quiet {
        return Err("`hello` reads `quiet 0`".to_owned());
    }
    if hello.session.is_none() {
        return Err("`hello` names no session token".to_owned());
    }
    let mode = hello.mode;
    let config = found
        .config
        .as_ref()
        .map_err(|e| format!("the configuration descriptor: {e}"))?;
    let eps = endpoints(config);
    for mi in [test_device::KEYBOARD_INTERFACE, test_device::PAD_INTERFACE] {
        let ep = eps
            .iter()
            .find(|e| e.interface == mi && e.address & 0x80 != 0 && e.attributes & 3 == 3)
            .ok_or_else(|| format!("no interrupt IN endpoint on interface {mi}"))?;
        if ep.interval != mode.rate.interval_ms() {
            return Err(format!(
                "interface {mi}'s bInterval is {}, not the mode byte's {}",
                ep.interval,
                mode.rate.interval_ms()
            ));
        }
    }
    Ok(())
}

/// `tapestry test-device devices [--out <json>]`: finds the one device,
/// asks its `hello` on the bound link, writes what it found (to `out`, a
/// new file, or standard output) and checks its shape.
///
/// # Errors
///
/// No device, several, or the first broken part of its shape.
pub fn devices(out: Option<&Path>) -> anyhow::Result<()> {
    let found = find_on(None)?;
    let (mut v, result) = match &found {
        None => (
            json!({ "present": false }),
            Err(format!(
                "no test device: the parent {} and its interfaces are not present",
                parent_prefix().trim_end_matches('\\')
            )),
        ),
        Some(f) => {
            let hello = Connection::bind(&System, Some(&f.port_path))
                .and_then(|mut c| c.check_hello(None).cloned())
                .map_err(|e| format!("{e:#}"));
            let shape = shape(f, hello.as_ref().map_err(String::as_str));
            let mut v = f.to_json();
            if let Some(obj) = v.as_object_mut() {
                obj.insert(
                    "hello".to_owned(),
                    match &hello {
                        Ok(h) => json!({ "line": h.line }),
                        Err(e) => json!({ "error": e }),
                    },
                );
            }
            (v, shape)
        }
    };
    if let Some(obj) = v.as_object_mut() {
        obj.insert(
            "shape".to_owned(),
            match &result {
                Ok(()) => json!("ok"),
                Err(e) => json!({ "error": e }),
            },
        );
    }
    let text = serde_json::to_string_pretty(&v)?;
    match out {
        Some(path) => {
            let mut file = std::fs::File::create_new(path)
                .with_context(|| format!("{} (a new file)", path.display()))?;
            file.write_all(text.as_bytes())
                .with_context(|| path.display().to_string())?;
        }
        None => println!("{text}"),
    }
    result.map_err(|e| anyhow::anyhow!("the device's shape: {e}"))
}

/// Bytes as lowercase hex.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A present USB device: its instance id and node.
type Parent = (String, u32);

/// The present USB devices whose instance id starts with `prefix`, each
/// with its port path and node.
fn present_with_ports(prefix: &str) -> anyhow::Result<Vec<(Option<String>, Parent)>> {
    Ok(present_usb_ids()?
        .into_iter()
        .filter(|id| id.to_ascii_uppercase().starts_with(prefix))
        .filter_map(|id| {
            let node = locate(&id)?;
            Some((port_path(node), (id, node)))
        })
        .collect())
}

/// The one present parent with its port path and node, on `bound` when
/// given.
fn parent_on(bound: Option<&str>) -> anyhow::Result<Option<(String, Parent)>> {
    let prefix = parent_prefix();
    pick(present_with_ports(&prefix)?, bound)
        .map_err(|r| anyhow::Error::new(r).context(format!("the test device ({prefix})")))
}

/// `GUID_DEVINTERFACE_COMPORT` (ntddser.h): the COM port interface a
/// serial driver registers for its device.
const GUID_DEVINTERFACE_COMPORT: GUID = GUID::from_u128(0x86e0_d1e0_8089_11d0_9ce4_0800_3e30_1f73);

/// The link of USB device `node`: its interface 2's COM port interface
/// path, with its port name. `None` while the interface is not there.
fn link_of(node: u32) -> Option<Link> {
    children(node).into_iter().find_map(|child| {
        let id = device_id(child)?;
        if mi_of(&id) != Some(test_device::LINK_INTERFACE) {
            return None;
        }
        let path = interface_list(&GUID_DEVINTERFACE_COMPORT, Some(&id))
            .ok()?
            .into_iter()
            .next()?;
        Some(Link {
            path,
            com: port_name(child),
        })
    })
}

/// The configuration manager's view of the device's link.
pub struct System;

impl Endpoints for System {
    fn find(&self, bound: Option<&str>) -> anyhow::Result<Option<(String, Option<Link>)>> {
        Ok(parent_on(bound)?.map(|(location, (_, node))| (location, link_of(node))))
    }

    fn open(&self, path: &str) -> anyhow::Result<Box<dyn Port>> {
        Ok(Box::new(ComPort::open(path)?))
    }

    fn location_of(&self, path: &str) -> anyhow::Result<Option<String>> {
        // The interface's device node, which must be interface 2 of a test
        // device's composite parent.
        let Some(function) = interface_instance(path) else {
            return Ok(None);
        };
        if mi_of(&function) != Some(test_device::LINK_INTERFACE) {
            return Ok(None);
        }
        let Some(parent) = locate(&function).and_then(parent_of) else {
            return Ok(None);
        };
        let is_device = device_id(parent)
            .is_some_and(|id| id.to_ascii_uppercase().starts_with(&parent_prefix()));
        if !is_device {
            return Ok(None);
        }
        port_path(parent)
            .map(Some)
            .with_context(|| format!("the location of {path}'s device cannot be read"))
    }
}

/// Finds the one device, on port path `bound` when given: `None` when no
/// parent is present.
///
/// # Errors
///
/// The configuration manager's list cannot be read, or a [`Refusal`]:
/// several parents, an unreadable port path, or a device on another port.
pub fn find_on(bound: Option<&str>) -> anyhow::Result<Option<Found>> {
    let Some((port_path, (parent, node))) = parent_on(bound)? else {
        return Ok(None);
    };
    let hardware = prop_strings(node, &DEVPKEY_Device_HardwareIds);
    let mut functions = Vec::new();
    let mut com = None;
    for child in children(node) {
        let Some(id) = device_id(child) else { continue };
        let Some(mi) = mi_of(&id) else { continue };
        let hid_node = children(child).into_iter().next();
        let hid = hid_node.and_then(device_id);
        let usage = hid_node.and_then(|n| usage_of(&prop_strings(n, &DEVPKEY_Device_HardwareIds)));
        if mi == test_device::LINK_INTERFACE {
            com = port_name(child);
        }
        functions.push(Function { mi, id, hid, usage });
    }
    functions.sort_by_key(|f| f.mi);
    let hub_node = parent_of(node);
    let hub = hub_node.and_then(device_id);
    let port = prop_u32(node, &DEVPKEY_Device_Address);
    let config = match (&hub, port) {
        (Some(hub), Some(port)) => config_descriptor(hub, port).map_err(|e| format!("{e:#}")),
        _ => Err("no hub or port".to_owned()),
    };
    Ok(Some(Found {
        revision: revision_of(&hardware),
        location: prop_string(node, &DEVPKEY_Device_LocationInfo),
        port_path,
        container: prop_guid(node, &DEVPKEY_Device_ContainerId),
        parent,
        hub,
        port,
        functions,
        com,
        raw: raw_sources(),
        config,
        at: qpc(),
    }))
}

fn present_usb_ids() -> anyhow::Result<Vec<String>> {
    let filter = w!("USB");
    let flags = CM_GETIDLIST_FILTER_ENUMERATOR | CM_GETIDLIST_FILTER_PRESENT;
    for _ in 0..4 {
        let mut len = 0;
        // SAFETY: one u32 written; the filter is a static string.
        let r = unsafe { CM_Get_Device_ID_List_SizeW(&mut len, filter, flags) };
        if r != CR_SUCCESS {
            bail!("CM_Get_Device_ID_List_Size: {}", r.0);
        }
        let mut buf = vec![0_u16; len as usize + 1];
        // SAFETY: the buffer's length is passed with it.
        let r = unsafe { CM_Get_Device_ID_ListW(filter, &mut buf, flags) };
        if r == CR_SUCCESS {
            return Ok(multi_sz(&buf));
        }
        // The list grew between the two calls: try again.
    }
    bail!("the device list kept changing")
}

fn locate(id: &str) -> Option<u32> {
    let id = wide(id);
    let mut node = 0;
    // SAFETY: a nul-terminated id; one u32 written.
    let r = unsafe { CM_Locate_DevNodeW(&mut node, PCWSTR(id.as_ptr()), CM_LOCATE_DEVNODE_NORMAL) };
    (r == CR_SUCCESS).then_some(node)
}

fn device_id(node: u32) -> Option<String> {
    let mut buf = [0_u16; 512];
    // SAFETY: the buffer's length is passed with it.
    let r = unsafe { CM_Get_Device_IDW(node, &mut buf, 0) };
    (r == CR_SUCCESS).then(|| until_nul(&buf))
}

fn children(node: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let mut child = 0;
    // SAFETY: one u32 written.
    if unsafe { CM_Get_Child(&mut child, node, 0) } != CR_SUCCESS {
        return out;
    }
    out.push(child);
    loop {
        let mut next = 0;
        // SAFETY: one u32 written.
        if unsafe { CM_Get_Sibling(&mut next, child, 0) } != CR_SUCCESS {
            return out;
        }
        out.push(next);
        child = next;
    }
}

fn parent_of(node: u32) -> Option<u32> {
    let mut parent = 0;
    // SAFETY: one u32 written.
    (unsafe { CM_Get_Parent(&mut parent, node, 0) } == CR_SUCCESS).then_some(parent)
}

fn prop_bytes(node: u32, key: &DEVPROPKEY) -> Option<Vec<u8>> {
    let mut kind = DEVPROPTYPE(0);
    let mut len = 0;
    // SAFETY: a size query: no buffer.
    let _ = unsafe { CM_Get_DevNode_PropertyW(node, key, &mut kind, None, &mut len, 0) };
    if len == 0 {
        return None;
    }
    let mut buf = vec![0_u8; len as usize];
    // SAFETY: the buffer holds `len` bytes.
    let r = unsafe {
        CM_Get_DevNode_PropertyW(node, key, &mut kind, Some(buf.as_mut_ptr()), &mut len, 0)
    };
    (r == CR_SUCCESS).then_some(buf)
}

fn utf16(bytes: &[u8]) -> Vec<u16> {
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&c| u16::from_le_bytes(c))
        .collect()
}

fn prop_string(node: u32, key: &DEVPROPKEY) -> Option<String> {
    prop_bytes(node, key).map(|b| until_nul(&utf16(&b)))
}

fn prop_strings(node: u32, key: &DEVPROPKEY) -> Vec<String> {
    prop_bytes(node, key).map_or_else(Vec::new, |b| multi_sz(&utf16(&b)))
}

fn prop_u32(node: u32, key: &DEVPROPKEY) -> Option<u32> {
    let b = prop_bytes(node, key)?;
    Some(u32::from_le_bytes(b.get(..4)?.try_into().ok()?))
}

fn prop_guid(node: u32, key: &DEVPROPKEY) -> Option<u128> {
    let b = prop_bytes(node, key)?;
    let bytes: [u8; 16] = b.get(..16)?.try_into().ok()?;
    // SAFETY: a GUID is 16 bytes of plain data.
    let guid: GUID = unsafe { std::mem::transmute(bytes) };
    Some(guid.to_u128())
}

/// The COM port name the serial driver gives function `node`.
fn port_name(node: u32) -> Option<String> {
    let mut key = HKEY::default();
    // SAFETY: one key written; closed below.
    let r = unsafe {
        CM_Open_DevNode_Key(
            node,
            KEY_READ.0,
            0,
            RegDisposition_OpenExisting,
            &mut key,
            CM_REGISTRY_HARDWARE,
        )
    };
    if r != CR_SUCCESS {
        return None;
    }
    let mut buf = [0_u16; 64];
    let mut len = u32::try_from(size_of_val(&buf)).unwrap_or(0);
    // SAFETY: the buffer holds `len` bytes.
    let read = unsafe {
        RegQueryValueExW(
            key,
            w!("PortName"),
            None,
            None,
            Some(buf.as_mut_ptr().cast::<u8>()),
            Some(&mut len),
        )
    };
    // SAFETY: the key was opened above.
    let _ = unsafe { RegCloseKey(key) };
    read.is_ok().then(|| until_nul(&buf))
}

/// The port path of USB device `node` ([`port_path_of`]).
fn port_path(node: u32) -> Option<String> {
    let hub = parent_of(node).and_then(device_id);
    let location = prop_string(node, &DEVPKEY_Device_LocationInfo);
    port_path_of(
        &prop_strings(node, &DEVPKEY_Device_LocationPaths),
        hub.as_deref().zip(location.as_deref()),
    )
}

/// The present interfaces of class `class`, of device `device` when given.
fn interface_list(class: &GUID, device: Option<&str>) -> anyhow::Result<Vec<String>> {
    let id = device.map(wide);
    let id = id.as_ref().map_or(PCWSTR::null(), |w| PCWSTR(w.as_ptr()));
    for _ in 0..4 {
        let mut len = 0;
        // SAFETY: a size query with a nul-terminated id or none.
        let r = unsafe {
            CM_Get_Device_Interface_List_SizeW(
                &mut len,
                class,
                id,
                CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
            )
        };
        if r != CR_SUCCESS {
            bail!("CM_Get_Device_Interface_List_Size: {}", r.0);
        }
        let mut buf = vec![0_u16; len as usize + 1];
        // SAFETY: the buffer's length is passed with it.
        let r = unsafe {
            CM_Get_Device_Interface_ListW(class, id, &mut buf, CM_GET_DEVICE_INTERFACE_LIST_PRESENT)
        };
        if r == CR_SUCCESS {
            return Ok(multi_sz(&buf));
        }
        // The list grew between the two calls: try again.
    }
    bail!("the interface list kept changing")
}

/// The interface path of hub `hub` (an instance id).
fn hub_path(hub: &str) -> anyhow::Result<String> {
    interface_list(&GUID_DEVINTERFACE_USB_HUB, Some(hub))?
        .into_iter()
        .next()
        .with_context(|| format!("hub {hub} has no hub interface"))
}

/// `GUID_DEVINTERFACE_DISK` (winioctl.h).
const GUID_DEVINTERFACE_DISK: GUID = GUID::from_u128(0x53f5_6307_b6bf_11d0_94f2_00a0_c91e_fb8b);
/// `IOCTL_STORAGE_GET_DEVICE_NUMBER` (winioctl.h): `CTL_CODE(0x2D, 0x420,
/// METHOD_BUFFERED, FILE_ANY_ACCESS)`.
const IOCTL_STORAGE_GET_DEVICE_NUMBER: u32 = 0x002D_1080;

/// The (device type, device number) of the storage device `path` opens
/// (`\\.\E:`, or a disk's interface path): `STORAGE_DEVICE_NUMBER`'s first
/// two fields.
fn storage_number(path: &str) -> Option<(u32, u32)> {
    let path = wide(path);
    // SAFETY: a nul-terminated path; no access asked, for a query only.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .ok()?;
    let handle = Owned(handle);
    // STORAGE_DEVICE_NUMBER: DeviceType, DeviceNumber, PartitionNumber.
    let mut out = [0_u32; 3];
    let mut got = 0;
    let len = u32::try_from(size_of_val(&out)).unwrap_or(0);
    // SAFETY: the output buffer holds `len` bytes; no input.
    unsafe {
        DeviceIoControl(
            handle.0,
            IOCTL_STORAGE_GET_DEVICE_NUMBER,
            None,
            0,
            Some(out.as_mut_ptr().cast::<c_void>()),
            len,
            Some(&mut got),
            None,
        )
    }
    .ok()?;
    let [kind, number, _] = out;
    Some((kind, number))
}

/// The device instance id of interface `path`.
fn interface_instance(path: &str) -> Option<String> {
    let path = wide(path);
    let mut kind = DEVPROPTYPE(0);
    let mut buf = [0_u16; 512];
    let mut len = u32::try_from(size_of_val(&buf)).unwrap_or(0);
    // SAFETY: a nul-terminated path; the buffer holds `len` bytes.
    let r = unsafe {
        CM_Get_Device_Interface_PropertyW(
            PCWSTR(path.as_ptr()),
            &DEVPKEY_Device_InstanceId,
            &mut kind,
            Some(buf.as_mut_ptr().cast::<u8>()),
            &mut len,
            0,
        )
    };
    (r == CR_SUCCESS).then(|| until_nul(&buf))
}

/// The location of the boot ROM's USB device that volume `volume`
/// (`\\?\Volume{…}\`) lies on: the volume's disk by its storage number
/// among the present disks, then the disk's ancestors in the configuration
/// manager's tree up to the `USB\VID_2E8A&PID_000F` device. `None` when any
/// link of that chain is missing.
pub fn volume_location(volume: &str) -> Option<String> {
    let want = storage_number(volume.strip_suffix('\\').unwrap_or(volume))?;
    let disk = interface_list(&GUID_DEVINTERFACE_DISK, None)
        .ok()?
        .into_iter()
        .find(|p| storage_number(p) == Some(want))?;
    let mut node = locate(&interface_instance(&disk)?)?;
    let prefix = boot_rom_prefix();
    // A disk sits a few levels under its USB device; the bound stops a
    // walk that never meets it.
    for _ in 0..8 {
        node = parent_of(node)?;
        if device_id(node)?.to_ascii_uppercase().starts_with(&prefix) {
            return port_path(node);
        }
    }
    None
}

/// The active configuration descriptor of the device on `port` of `hub`,
/// as the hub reads it from the device.
fn config_descriptor(hub: &str, port: u32) -> anyhow::Result<Vec<u8>> {
    let path = wide(&hub_path(hub)?);
    // SAFETY: a nul-terminated path; no security attributes or template.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            GENERIC_WRITE.0,
            FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .context("opening the hub")?;
    let hub = Owned(handle);
    let request = |length: u16| -> anyhow::Result<Vec<u8>> {
        // USB_DESCRIPTOR_REQUEST: ConnectionIndex, then the setup packet
        // (GET_DESCRIPTOR of configuration 0), then the data.
        let mut buf = Vec::with_capacity(12 + usize::from(length));
        buf.extend_from_slice(&port.to_le_bytes());
        buf.extend_from_slice(&[0x80, 6, 0, 2, 0, 0]);
        buf.extend_from_slice(&length.to_le_bytes());
        buf.resize(12 + usize::from(length), 0);
        let mut got = 0;
        let len = u32::try_from(buf.len()).unwrap_or(0);
        let ptr = buf.as_mut_ptr().cast::<c_void>();
        // SAFETY: the buffer serves as input and output, `len` bytes each.
        unsafe {
            DeviceIoControl(
                hub.0,
                IOCTL_USB_GET_DESCRIPTOR_FROM_NODE_CONNECTION,
                Some(ptr.cast_const()),
                len,
                Some(ptr),
                len,
                Some(&mut got),
                None,
            )
        }
        .context("IOCTL_USB_GET_DESCRIPTOR_FROM_NODE_CONNECTION")?;
        Ok(buf.get(12..got as usize).unwrap_or_default().to_vec())
    };
    let head = request(9)?;
    let total = head
        .get(2..4)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .context("short configuration descriptor")?;
    request(total)
}

/// The device's Raw Input devices.
pub fn raw_sources() -> Vec<RawSource> {
    let size = u32::try_from(size_of::<RAWINPUTDEVICELIST>()).unwrap_or(0);
    let mut count = 0;
    // SAFETY: a count query.
    let _ = unsafe { GetRawInputDeviceList(None, &mut count, size) };
    let mut list = vec![RAWINPUTDEVICELIST::default(); count as usize + 8];
    let mut cap = u32::try_from(list.len()).unwrap_or(0);
    // SAFETY: the list holds `cap` entries.
    let got = unsafe { GetRawInputDeviceList(Some(list.as_mut_ptr()), &mut cap, size) };
    if got == u32::MAX {
        return Vec::new();
    }
    list.truncate(got as usize);
    let vid_pid = format!("VID_{:04X}&PID_{:04X}", test_device::VID, test_device::PID);
    list.iter()
        .filter_map(|d| {
            let path = device_name(d.hDevice)?;
            if !path.to_ascii_uppercase().contains(&vid_pid) {
                return None;
            }
            let instance = instance_of_path(&path);
            let container =
                locate(&instance).and_then(|n| prop_guid(n, &DEVPKEY_Device_ContainerId));
            let caps = (d.dwType == RIM_TYPEHID)
                .then(|| hid_caps(d.hDevice))
                .flatten();
            Some(RawSource {
                handle: d.hDevice.0 as usize,
                mi: mi_of(&path),
                keyboard: d.dwType == RIM_TYPEKEYBOARD,
                path,
                instance,
                container,
                caps,
            })
        })
        .collect()
}

/// The interface path of Raw Input device `handle`.
pub fn device_name(handle: HANDLE) -> Option<String> {
    let mut buf = [0_u16; 512];
    let mut chars = u32::try_from(buf.len()).unwrap_or(0);
    // SAFETY: the buffer holds `chars` characters.
    let got = unsafe {
        GetRawInputDeviceInfoW(
            Some(handle),
            RIDI_DEVICENAME,
            Some(buf.as_mut_ptr().cast::<c_void>()),
            &mut chars,
        )
    };
    (got != 0 && got != u32::MAX).then(|| until_nul(&buf))
}

fn hid_caps(handle: HANDLE) -> Option<HidCaps> {
    let mut size = 0;
    // SAFETY: a size query.
    let _ = unsafe { GetRawInputDeviceInfoW(Some(handle), RIDI_PREPARSEDDATA, None, &mut size) };
    if size == 0 {
        return None;
    }
    // u64s, for the preparsed data's alignment.
    let mut data = vec![0_u64; (size as usize).div_ceil(8)];
    // SAFETY: the buffer holds at least `size` bytes.
    let got = unsafe {
        GetRawInputDeviceInfoW(
            Some(handle),
            RIDI_PREPARSEDDATA,
            Some(data.as_mut_ptr().cast::<c_void>()),
            &mut size,
        )
    };
    if got == 0 || got == u32::MAX {
        return None;
    }
    let pp = PHIDP_PREPARSED_DATA(data.as_ptr() as isize);
    let mut caps = HIDP_CAPS::default();
    // SAFETY: valid preparsed data; one struct written.
    if unsafe { HidP_GetCaps(pp, &mut caps) } != HIDP_STATUS_SUCCESS {
        return None;
    }
    let mut buttons = vec![HIDP_BUTTON_CAPS::default(); usize::from(caps.NumberInputButtonCaps)];
    let mut n = caps.NumberInputButtonCaps;
    // SAFETY: the array holds `n` entries.
    let b_ok = n > 0
        && unsafe { HidP_GetButtonCaps(HidP_Input, buttons.as_mut_ptr(), &mut n, pp) }
            == HIDP_STATUS_SUCCESS;
    let button_range = b_ok
        .then(|| {
            buttons.iter().take(usize::from(n)).find_map(|b| {
                if b.UsagePage == input::hid::BUTTON_PAGE && b.IsRange {
                    // SAFETY: `IsRange` says which member of the union is set.
                    Some(unsafe { (b.Anonymous.Range.UsageMin, b.Anonymous.Range.UsageMax) })
                } else {
                    None
                }
            })
        })
        .flatten();
    let mut values = vec![HIDP_VALUE_CAPS::default(); usize::from(caps.NumberInputValueCaps)];
    let mut m = caps.NumberInputValueCaps;
    // SAFETY: the array holds `m` entries.
    let v_ok = m > 0
        && unsafe { HidP_GetValueCaps(HidP_Input, values.as_mut_ptr(), &mut m, pp) }
            == HIDP_STATUS_SUCCESS;
    let x = v_ok
        .then(|| {
            values.iter().take(usize::from(m)).find_map(|v| {
                // SAFETY: `IsRange` says which member of the union is set.
                let usage = unsafe {
                    if v.IsRange {
                        v.Anonymous.Range.UsageMin
                    } else {
                        v.Anonymous.NotRange.Usage
                    }
                };
                (v.UsagePage == input::hid::DESKTOP_PAGE && usage == 0x30)
                    .then_some((v.LogicalMin, v.LogicalMax))
            })
        })
        .flatten();
    Some(HidCaps {
        usage_page: caps.UsagePage,
        usage: caps.Usage,
        input_len: caps.InputReportByteLength,
        buttons: button_range,
        x,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interface_paths_name_their_instance_and_interface() {
        let path =
            r"\\?\HID#VID_1209&PID_0001&MI_01#7&2a5e&0&0000#{4d1e55b2-f16f-11cf-88cb-001111000030}";
        assert_eq!(
            instance_of_path(path),
            r"HID\VID_1209&PID_0001&MI_01\7&2a5e&0&0000"
        );
        assert_eq!(mi_of(path), Some(1));
        assert_eq!(mi_of(r"USB\VID_1209&PID_0001\6&1"), None);
        assert_eq!(parent_prefix(), r"USB\VID_1209&PID_0001\");
    }

    const PORT_A: &str = "PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(3)";
    const PORT_B: &str = "PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(4)";

    fn on(port: Option<&str>, name: &'static str) -> (Option<String>, &'static str) {
        (port.map(str::to_owned), name)
    }

    #[test]
    fn the_one_candidate_is_taken_and_binds_its_port() {
        assert_eq!(pick(Vec::<(Option<String>, ())>::new(), None), Ok(None));
        assert_eq!(
            pick(Vec::<(Option<String>, ())>::new(), Some(PORT_A)),
            Ok(None)
        );
        assert_eq!(
            pick(vec![on(Some(PORT_A), "a")], None),
            Ok(Some((PORT_A.to_owned(), "a")))
        );
        assert_eq!(
            pick(vec![on(Some(PORT_A), "a")], Some(PORT_A)),
            Ok(Some((PORT_A.to_owned(), "a")))
        );
    }

    #[test]
    fn several_devices_are_refused() {
        let both = || vec![on(Some(PORT_A), "a"), on(Some(PORT_B), "b")];
        let several = Err(Refusal::Several(vec![
            Some(PORT_A.to_owned()),
            Some(PORT_B.to_owned()),
        ]));
        assert_eq!(pick(both(), None), several);
        assert_eq!(pick(both(), Some(PORT_A)), several);
    }

    #[test]
    fn a_candidate_on_another_port_or_on_none_known_is_refused() {
        assert_eq!(
            pick(vec![on(Some(PORT_B), "b")], Some(PORT_A)),
            Err(Refusal::OtherPort {
                found: PORT_B.to_owned(),
                bound: PORT_A.to_owned()
            })
        );
        assert_eq!(pick(vec![on(None, "a")], None), Err(Refusal::NoPort));
        assert_eq!(
            pick(vec![on(None, "a")], Some(PORT_A)),
            Err(Refusal::NoPort)
        );
    }

    #[test]
    fn the_port_path_prefers_the_location_paths() {
        let hub = (r"USB\ROOT_HUB30\4&1", "Port_#0003.Hub_#0001");
        assert_eq!(
            port_path_of(
                &[PORT_A.to_owned(), "ACPI(_SB_)#USB(3)".to_owned()],
                Some(hub)
            ),
            Some(PORT_A.to_owned())
        );
        assert_eq!(
            port_path_of(&[], Some(hub)),
            Some(r"USB\ROOT_HUB30\4&1|Port_#0003.Hub_#0001".to_owned())
        );
        assert_eq!(port_path_of(&[], None), None);
        assert_eq!(boot_rom_prefix(), r"USB\VID_2E8A&PID_000F\");
    }

    #[test]
    fn hardware_ids_give_the_usage_and_the_revision() {
        let ids = [
            r"HID\VID_1209&PID_0001&REV_0108&MI_01".to_owned(),
            "HID_DEVICE_UP:0001_U:0005".to_owned(),
            "HID_DEVICE".to_owned(),
        ];
        assert_eq!(usage_of(&ids), Some((1, 5)));
        assert_eq!(revision_of(&ids), Some(0x0108));
        assert_eq!(usage_of(&ids[2..]), None);
    }

    #[test]
    fn a_configuration_lists_each_endpoint_with_its_interface() {
        let config = [
            9, 2, 0, 0, 4, 1, 0, 0x80, 50, // configuration
            9, 4, 0, 0, 1, 3, 0, 0, 0, // interface 0
            9, 0x21, 0x11, 1, 0, 1, 0x22, 0, 0, // HID
            7, 5, 0x81, 3, 64, 0, 8, // endpoint 1 IN, interval 8
            9, 4, 1, 0, 1, 3, 0, 0, 0, // interface 1
            7, 5, 0x82, 3, 64, 0, 1, // endpoint 2 IN, interval 1
        ];
        assert_eq!(
            endpoints(&config),
            [
                Endpoint {
                    interface: 0,
                    address: 0x81,
                    attributes: 3,
                    max_packet: 64,
                    interval: 8,
                },
                Endpoint {
                    interface: 1,
                    address: 0x82,
                    attributes: 3,
                    max_packet: 64,
                    interval: 1,
                },
            ]
        );
        // A zero length ends the walk instead of looping.
        assert!(endpoints(&[0, 2, 9]).is_empty());
    }

    /// The device as F1 expects it, at mode byte 8 (125 Hz on change).
    fn good() -> (Found, Facts) {
        let container = Some(0x1234_u128);
        let config = vec![
            9, 2, 0, 0, 4, 1, 0, 0x80, 50, // configuration
            9, 4, 0, 0, 1, 3, 0, 0, 0, // interface 0
            7, 5, 0x81, 3, 64, 0, 8, // endpoint 1 IN, interval 8
            9, 4, 1, 0, 1, 3, 0, 0, 0, // interface 1
            7, 5, 0x82, 3, 64, 0, 8, // endpoint 2 IN, interval 8
        ];
        let function = |mi, usage| Function {
            mi,
            id: format!(r"USB\VID_1209&PID_0001&MI_{mi:02X}\7&1"),
            hid: None,
            usage,
        };
        let found = Found {
            parent: r"USB\VID_1209&PID_0001\6&1".to_owned(),
            revision: Some(0x0108),
            location: None,
            port_path: PORT_A.to_owned(),
            hub: None,
            port: Some(3),
            container,
            functions: vec![
                function(0, Some((1, 6))),
                function(1, Some((1, 5))),
                function(2, None),
            ],
            com: Some("COM5".to_owned()),
            raw: vec![
                RawSource {
                    handle: 1,
                    path: r"\?\HID#VID_1209&PID_0001&MI_00#7&1#{x}".to_owned(),
                    instance: String::new(),
                    keyboard: true,
                    mi: Some(0),
                    container,
                    caps: None,
                },
                RawSource {
                    handle: 2,
                    path: r"\?\HID#VID_1209&PID_0001&MI_01#7&2#{x}".to_owned(),
                    instance: String::new(),
                    keyboard: false,
                    mi: Some(1),
                    container,
                    caps: Some(HidCaps {
                        usage_page: 1,
                        usage: 5,
                        input_len: 19,
                        buttons: Some((1, 16)),
                        x: Some((0, 255)),
                    }),
                },
            ],
            config: Ok(config),
            at: 0,
        };
        let hello = super::super::link::hello_facts(
            &super::super::link::fake::hello("0123456789ab", false),
            None,
        )
        .unwrap();
        (found, hello)
    }

    /// The shape names the first broken part; a pad report of another
    /// length is one.
    #[test]
    fn a_wrong_report_length_fails_the_shape() {
        let (found, hello) = good();
        assert_eq!(shape(&found, Ok(&hello)), Ok(()));
        let mut short = found.clone();
        if let Some(caps) = short.raw.get_mut(1).and_then(|r| r.caps.as_mut()) {
            caps.input_len = 18;
        }
        let e = shape(&short, Ok(&hello)).unwrap_err();
        assert!(e.contains("input report is 18 bytes, not 19"), "{e}");
    }

    #[test]
    fn each_other_broken_part_fails_the_shape_by_name() {
        let (found, hello) = good();
        let broken = |f: &dyn Fn(&mut Found, &mut Facts), want: &str| {
            let (mut found, mut hello) = (found.clone(), hello.clone());
            f(&mut found, &mut hello);
            let e = shape(&found, Ok(&hello)).unwrap_err();
            assert!(e.contains(want), "{e} lacks {want}");
        };
        broken(&|f, _| f.functions.retain(|x| x.mi != 0), "no interface 0");
        broken(
            &|f, _| f.raw.retain(|r| !r.keyboard),
            "no Raw Input keyboard",
        );
        broken(
            &|f, _| {
                if let Some(c) = f.raw.get_mut(1).and_then(|r| r.caps.as_mut()) {
                    c.buttons = Some((1, 8));
                }
            },
            "buttons",
        );
        broken(&|f, _| f.com = None, "no COM port");
        broken(&|_, h| h.faults = true, "debug faults");
        broken(&|_, h| h.quiet = false, "quiet 0");
        broken(&|_, h| h.session = None, "no session");
        broken(
            &|_, h| h.mode = test_device::mode::Mode::from_byte(1).unwrap(),
            "bInterval is 8, not the mode byte's 1",
        );
        broken(&|f, _| f.raw[1].container = None, "container");
        assert!(
            shape(&found, Err("no answer"))
                .unwrap_err()
                .contains("no answer")
        );
    }
}
