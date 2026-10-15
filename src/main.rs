//! usbwhere: for every USB device, shows the driver bound to each interface
//! and where to access it (/dev/sdX, /dev/videoN, /dev/ttyACMN, ...).
//!
//! The current state is read straight from /sys; `--watch` listens to udev
//! events (via the `udev` crate).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::sync::OnceLock;

use udev::{EventType, MonitorBuilder};

const SYS_USB: &str = "/sys/bus/usb/devices";
const MAX_DEPTH: usize = 10;
/// Quiet period (ms) after the last udev event before a device is printed:
/// gives drivers time to create their nodes.
const DEBOUNCE_MS: i32 = 700;

const USB_IDS_PATHS: [&str; 5] = [
    "/usr/share/hwdata/usb.ids",
    "/usr/share/misc/usb.ids",
    "/var/lib/usbutils/usb.ids",
    "/usr/share/usb.ids",
    "/usr/share/libosinfo/usb.ids",
];

type Mounts = Vec<(String, String)>;

// ---------- colors (plain ANSI, no dependency) ----------

static COLOR: OnceLock<bool> = OnceLock::new();

/// Auto mode: color only when stdout is a terminal and NO_COLOR is not set.
fn auto_color() -> bool {
    std::env::var_os("NO_COLOR").is_none() && unsafe { libc::isatty(1) } == 1
}

fn use_color() -> bool {
    *COLOR.get_or_init(auto_color)
}

fn paint(code: &str, s: &str) -> String {
    if use_color() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

fn bold(s: &str) -> String {
    paint("1", s)
}
fn dim(s: &str) -> String {
    paint("2", s)
}
fn red(s: &str) -> String {
    paint("1;31", s)
}
fn green(s: &str) -> String {
    paint("32", s)
}
fn yellow(s: &str) -> String {
    paint("33", s)
}
fn blue(s: &str) -> String {
    paint("34", s)
}
fn magenta(s: &str) -> String {
    paint("35", s)
}
fn cyan(s: &str) -> String {
    paint("36", s)
}

// ---------- data model ----------

struct Interface {
    number: u8,
    class: u8,
    subclass: u8,
    protocol: u8,
    driver: Option<String>,
    nodes: Vec<String>,
}

struct Device {
    sysname: String,
    bus: u32,
    devnum: u32,
    vid: String,
    pid: String,
    vendor: String,
    product: String,
    serial: Option<String>,
    speed: String,
    interfaces: Vec<Interface>,
}

impl Device {
    fn label(&self) -> String {
        format!("{}:{} {} {}", self.vid, self.pid, self.vendor, self.product)
            .replace("  ", " ")
            .trim()
            .to_string()
    }
}

// ---------- usb.ids database (vendor / product names) ----------

#[derive(Default)]
struct UsbIds {
    vendors: HashMap<u16, String>,
    products: HashMap<(u16, u16), String>,
}

fn parse_id(s: &str) -> Option<(u16, String)> {
    let (id, name) = s.split_once("  ")?;
    Some((u16::from_str_radix(id.trim(), 16).ok()?, name.trim().to_string()))
}

impl UsbIds {
    fn load() -> Self {
        USB_IDS_PATHS
            .iter()
            .find_map(|p| fs::read_to_string(p).ok())
            .map(|c| Self::parse(&c))
            .unwrap_or_default()
    }

    fn parse(content: &str) -> Self {
        let mut ids = UsbIds::default();
        let mut current: Option<u16> = None;
        for line in content.lines() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line.starts_with("C ") {
                break; // end of the vendor section
            }
            if let Some(rest) = line.strip_prefix('\t') {
                if rest.starts_with('\t') {
                    continue; // interface line
                }
                if let (Some(v), Some((id, name))) = (current, parse_id(rest)) {
                    ids.products.insert((v, id), name);
                }
            } else if let Some((id, name)) = parse_id(line) {
                current = Some(id);
                ids.vendors.insert(id, name);
            }
        }
        ids
    }

    fn vendor(&self, vid: &str) -> Option<String> {
        let v = u16::from_str_radix(vid, 16).ok()?;
        self.vendors.get(&v).cloned()
    }

    fn product(&self, vid: &str, pid: &str) -> Option<String> {
        let v = u16::from_str_radix(vid, 16).ok()?;
        let p = u16::from_str_radix(pid, 16).ok()?;
        self.products.get(&(v, p)).cloned()
    }
}

// ---------- reading sysfs ----------

fn attr(dir: &Path, name: &str) -> Option<String> {
    let s = fs::read_to_string(dir.join(name)).ok()?;
    let s = s.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

fn attr_hex(dir: &Path, name: &str) -> u8 {
    attr(dir, name)
        .and_then(|s| u8::from_str_radix(&s, 16).ok())
        .unwrap_or(0)
}

fn attr_dec(dir: &Path, name: &str) -> u32 {
    attr(dir, name).and_then(|s| s.parse().ok()).unwrap_or(0)
}

fn uevent_value(dir: &Path, key: &str) -> Option<String> {
    let content = fs::read_to_string(dir.join("uevent")).ok()?;
    let prefix = format!("{key}=");
    content
        .lines()
        .find_map(|l| l.strip_prefix(&prefix).map(|v| v.to_string()))
}

fn read_mounts() -> Mounts {
    let Ok(content) = fs::read_to_string("/proc/mounts") else {
        return Vec::new();
    };
    content
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let dev = f.next()?;
            let mp = f.next()?;
            if dev.starts_with("/dev/") {
                Some((dev.to_string(), mp.replace("\\040", " ")))
            } else {
                None
            }
        })
        .collect()
}

// ---------- presentation helpers ----------

fn class_name(class: u8, sub: u8, proto: u8) -> &'static str {
    match class {
        0x01 => "Audio",
        0x02 => "Communications (CDC)",
        0x03 => "HID (keyboard/mouse/gamepad...)",
        0x05 => "Physical",
        0x06 => "Image (PTP/camera)",
        0x07 => "Printer",
        0x08 => "Mass storage",
        0x09 => "Hub",
        0x0a => "CDC-Data",
        0x0b => "Smart card",
        0x0d => "Content security",
        0x0e => "Video (UVC)",
        0x0f => "Personal healthcare",
        0x10 => "Audio/Video",
        0xe0 if sub == 1 && proto == 1 => "Bluetooth",
        0xe0 => "Wireless",
        0xef => "Miscellaneous",
        0xfe => "Application specific (DFU...)",
        0xff => "Vendor specific",
        _ => "Other",
    }
}

fn mode_string(mode: u32) -> String {
    let kind = match mode & 0o170000 {
        0o060000 => 'b',
        0o020000 => 'c',
        0o040000 => 'd',
        _ => '-',
    };
    let mut s = String::from(kind);
    for (i, c) in "rwxrwxrwx".chars().enumerate() {
        s.push(if mode & (1 << (8 - i)) != 0 { c } else { '-' });
    }
    s
}

fn lookup_name(file: &str, id: u32) -> String {
    fs::read_to_string(file)
        .ok()
        .and_then(|c| {
            c.lines().find_map(|l| {
                let mut f = l.split(':');
                let name = f.next()?;
                f.next()?;
                let n: u32 = f.next()?.parse().ok()?;
                (n == id).then(|| name.to_string())
            })
        })
        .unwrap_or_else(|| id.to_string())
}

fn describe_dev(devname: &str, mounts: &Mounts) -> String {
    let path = format!("/dev/{devname}");
    let mut s = bold(&green(&path));
    match fs::metadata(&path) {
        Ok(m) => {
            let info = format!(
                "[{} {}:{}]",
                mode_string(m.mode()),
                lookup_name("/etc/passwd", m.uid()),
                lookup_name("/etc/group", m.gid())
            );
            s += &format!("  {}", dim(&info));
        }
        Err(_) => s += &format!("  {}", red("[not in /dev]")),
    }
    for (dev, mp) in mounts {
        if *dev == path {
            s += &format!("  {}", magenta(&format!("→ mounted on {mp}")));
        }
    }
    s
}

// ---------- discovering the nodes of an interface ----------

/// Walks the sysfs directory of an interface (without following symlinks)
/// and collects everything that gives access to the device:
/// - a `uevent` file with DEVNAME= → a /dev node
/// - a net/, bluetooth/ or sound/cardN directory → a resource without /dev node
fn collect_nodes(dir: &Path, depth: usize, mounts: &Mounts, out: &mut Vec<String>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    let parent = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    for e in entries {
        let Ok(ft) = e.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        if name == "power" || name.starts_with("ep_") || name == "bsg" {
            continue;
        }
        let path = e.path();

        let special = match parent.as_str() {
            "net" => Some(format!("network interface {}", bold(&green(&name)))),
            "bluetooth" => Some(format!("Bluetooth controller {}", bold(&green(&name)))),
            "sound" if name.starts_with("card") => {
                Some(format!("sound card {} (ALSA)", bold(&green(&name))))
            }
            _ => None,
        };
        if let Some(s) = special {
            if !out.contains(&s) {
                out.push(s);
            }
        }
        if let Some(devname) = uevent_value(&path, "DEVNAME") {
            let s = describe_dev(&devname, mounts);
            if !out.contains(&s) {
                out.push(s);
            }
        }
        collect_nodes(&path, depth + 1, mounts, out);
    }
}

// ---------- loading devices ----------

fn load_device(sysname: &str, show_hubs: bool, mounts: &Mounts, ids: &UsbIds) -> Option<Device> {
    let dir = Path::new(SYS_USB).join(sysname);
    let vid = attr(&dir, "idVendor")?;
    let pid = attr(&dir, "idProduct")?;
    if attr_hex(&dir, "bDeviceClass") == 9 && !show_hubs {
        return None;
    }

    let prefix = format!("{sysname}:");
    let mut iface_names: Vec<String> = fs::read_dir(SYS_USB)
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(&prefix))
        .collect();
    iface_names.sort();

    let mut interfaces: Vec<Interface> = iface_names
        .iter()
        .map(|n| {
            let idir = Path::new(SYS_USB).join(n);
            let driver = fs::read_link(idir.join("driver"))
                .ok()
                .and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()));
            let mut nodes = Vec::new();
            collect_nodes(&idir, 0, mounts, &mut nodes);
            Interface {
                number: attr_hex(&idir, "bInterfaceNumber"),
                class: attr_hex(&idir, "bInterfaceClass"),
                subclass: attr_hex(&idir, "bInterfaceSubClass"),
                protocol: attr_hex(&idir, "bInterfaceProtocol"),
                driver,
                nodes,
            }
        })
        .collect();
    interfaces.sort_by_key(|i| i.number);

    let vendor = attr(&dir, "manufacturer")
        .or_else(|| ids.vendor(&vid))
        .unwrap_or_default();
    let product = attr(&dir, "product")
        .or_else(|| ids.product(&vid, &pid))
        .unwrap_or_else(|| "(unnamed)".into());

    Some(Device {
        sysname: sysname.to_string(),
        bus: attr_dec(&dir, "busnum"),
        devnum: attr_dec(&dir, "devnum"),
        vid,
        pid,
        vendor,
        product,
        serial: attr(&dir, "serial"),
        speed: attr(&dir, "speed").unwrap_or_else(|| "?".into()),
        interfaces,
    })
}

fn device_names() -> Vec<String> {
    let Ok(rd) = fs::read_dir(SYS_USB) else {
        return Vec::new();
    };
    let mut v: Vec<String> = rd
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.contains(':'))
        .collect();
    v.sort();
    v
}

fn load_all(show_hubs: bool, ids: &UsbIds) -> Vec<Device> {
    let mounts = read_mounts();
    let mut devs: Vec<Device> = device_names()
        .iter()
        .filter_map(|n| load_device(n, show_hubs, &mounts, ids))
        .collect();
    devs.sort_by_key(|d| (d.bus, d.devnum));
    devs
}

fn print_device(d: &Device) {
    let serial = d
        .serial
        .as_deref()
        .map(|s| format!("  {}", dim(&format!("serial={s}"))))
        .unwrap_or_default();
    println!(
        "{} {}  {}  {}{}",
        dim(&format!("Bus {:03} Device {:03}", d.bus, d.devnum)),
        yellow(&format!("{}:{}", d.vid, d.pid)),
        bold(&d.vendor),
        bold(&d.product),
        serial
    );
    println!(
        "   {}",
        dim(&format!(
            "{} Mb/s · raw access (libusb): /dev/bus/usb/{:03}/{:03}",
            d.speed, d.bus, d.devnum
        ))
    );
    let n = d.interfaces.len();
    for (i, itf) in d.interfaces.iter().enumerate() {
        let (branch, pad) = if i + 1 == n {
            ("└─", "   ")
        } else {
            ("├─", "│  ")
        };
        let driver = match &itf.driver {
            Some(dr) => green(&format!("driver={dr}")),
            None => red("⚠ no driver bound"),
        };
        println!(
            " {} {} · {} · {driver}",
            dim(branch),
            cyan(&format!("Interface {}", itf.number)),
            blue(class_name(itf.class, itf.subclass, itf.protocol)),
        );
        for node in &itf.nodes {
            println!(" {}   {} {node}", dim(pad), dim("→"));
        }
        if itf.nodes.is_empty() && itf.driver.is_some() {
            println!(" {}   {}", dim(pad), dim("(no node exposed)"));
        }
    }
    println!();
}

// ---------- --watch mode (udev events) ----------

/// Sysfs name of a USB device ("1-2", "1-2.3"), not of an interface
/// ("1-2:1.0") nor of a root hub ("usb1").
fn is_usb_device_name(s: &str) -> bool {
    s.contains('-')
        && s.chars().next().is_some_and(|c| c.is_ascii_digit())
        && s.chars().all(|c| c.is_ascii_digit() || c == '-' || c == '.')
}

/// The (deepest) USB device that contains this sysfs path, whatever the
/// subsystem of the event (block, video4linux, tty, sound...).
fn usb_device_of(path: &Path) -> Option<String> {
    path.components()
        .filter_map(|c| c.as_os_str().to_str())
        .rfind(|c| is_usb_device_name(c))
        .map(str::to_string)
}

fn flush_pending(
    pending: &mut BTreeSet<String>,
    shown: &mut BTreeMap<String, String>,
    show_hubs: bool,
    ids: &UsbIds,
) {
    let mounts = read_mounts();
    for name in std::mem::take(pending) {
        let Some(d) = load_device(&name, show_hubs, &mounts, ids) else {
            continue;
        };
        let title = if shown.contains_key(&name) {
            yellow("~ updated:")
        } else {
            green("+ plugged in:")
        };
        println!("{}", bold(&title));
        print_device(&d);
        shown.insert(name, d.label());
    }
}

fn watch(show_hubs: bool, ids: &UsbIds) -> io::Result<()> {
    let socket = MonitorBuilder::new()?.listen()?;
    let fd = socket.as_raw_fd();

    let mut shown: BTreeMap<String, String> = load_all(show_hubs, ids)
        .iter()
        .map(|d| (d.sysname.clone(), d.label()))
        .collect();
    let mut pending: BTreeSet<String> = BTreeSet::new();

    println!("{}\n", dim("Listening for udev events (Ctrl-C to quit)..."));
    loop {
        // nothing pending: sleep until an event; otherwise wait for silence
        let timeout = if pending.is_empty() { -1 } else { DEBOUNCE_MS };
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let r = unsafe { libc::poll(&mut pfd, 1, timeout) };
        if r < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if r == 0 {
            flush_pending(&mut pending, &mut shown, show_hubs, ids);
            continue;
        }

        for ev in socket.iter() {
            let path = ev.syspath().to_path_buf();
            let Some(name) = usb_device_of(&path) else {
                continue;
            };
            let is_the_device = path
                .file_name()
                .is_some_and(|n| n.to_str() == Some(name.as_str()));
            match ev.event_type() {
                EventType::Remove if is_the_device => {
                    pending.remove(&name);
                    if let Some(label) = shown.remove(&name) {
                        println!("{} {label} ({name})\n", red("- removed:"));
                    }
                }
                EventType::Remove | EventType::Unbind | EventType::Unknown => {}
                _ => {
                    pending.insert(name);
                }
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!(
            "usbwhere: where can I reach each USB device?\n\n\
             Usage: usbwhere [--all] [--watch] [--color | --no-color]\n\n\
             \x20 --all       include hubs (root hubs too)\n\
             \x20 --watch     listen to udev and print plug/unplug events\n\
             \x20 --color     force colors even when not writing to a terminal\n\
             \x20 --no-color  disable colors (NO_COLOR is also honored)"
        );
        return;
    }
    let all = args.iter().any(|a| a == "--all");
    if args.iter().any(|a| a == "--no-color") {
        let _ = COLOR.set(false);
    } else if args.iter().any(|a| a == "--color") {
        let _ = COLOR.set(true);
    }

    if !Path::new(SYS_USB).exists() {
        eprintln!("{SYS_USB} not found: no USB bus visible (or not running Linux).");
        std::process::exit(1);
    }

    let ids = UsbIds::load();

    if args.iter().any(|a| a == "--watch") {
        if let Err(e) = watch(all, &ids) {
            eprintln!("Cannot listen to udev: {e}");
            std::process::exit(1);
        }
        return;
    }

    let devs = load_all(all, &ids);
    if devs.is_empty() {
        println!("No USB devices (hubs excluded). Use --all to show everything.");
    }
    for d in &devs {
        print_device(d);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usb_ids_parse() {
        let ids = UsbIds::parse(
            "# comment\n046d  Logitech, Inc.\n\t0825  Webcam C270\n\t\t00  interface\n0781  SanDisk Corp.\n\t5581  Ultra\n\nC 00  (Defined at Interface level)\n",
        );
        assert_eq!(ids.vendor("046d").as_deref(), Some("Logitech, Inc."));
        assert_eq!(ids.product("046d", "0825").as_deref(), Some("Webcam C270"));
        assert_eq!(ids.product("0781", "5581").as_deref(), Some("Ultra"));
        assert_eq!(ids.vendor("0000"), None);
    }

    #[test]
    fn device_of_path() {
        let p = Path::new("/sys/devices/pci0000:00/0000:00:14.0/usb1/1-2/1-2.3/1-2.3:1.0/host6/target6:0:0/6:0:0:0/block/sdb");
        assert_eq!(usb_device_of(p).as_deref(), Some("1-2.3"));
        let p = Path::new("/sys/devices/pci0000:00/0000:00:14.0/usb1");
        assert_eq!(usb_device_of(p), None);
    }

    #[test]
    fn modes() {
        assert_eq!(mode_string(0o020660), "crw-rw----");
    }

    #[test]
    fn colors_can_be_disabled() {
        // paint() is a no-op once COLOR is set to false
        let _ = COLOR.set(false);
        assert_eq!(green("x"), "x");
    }
}
