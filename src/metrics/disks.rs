//! Physical disks: model, bus, size, serial, firmware, SSD / TRIM and temperature through
//! IOCTL_STORAGE_QUERY_PROPERTY on \\.\PhysicalDriveN opened without access rights (no
//! elevation needed). Whatever a disk does not answer stays empty.

use crate::util::{pcwstr, wide};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::*;

#[derive(Clone, Debug, Default)]
pub struct PhysicalDisk {
    /// PhysicalDriveN index, matches the leading number of PDH "PhysicalDisk" instances ("0 C:")
    pub index: u32,
    pub model: String,
    pub bus: &'static str,
    pub size: u64,
    /// serial number and firmware revision; empty when the disk does not report them
    pub serial: String,
    pub firmware: String,
    /// Some(true) for an SSD (no seek penalty), Some(false) for a spinning disk
    pub ssd: Option<bool>,
    /// TRIM enabled for the disk
    pub trim: Option<bool>,
    /// °C (popup only, Windows 10+ and disks that report it)
    pub temp: Option<f32>,
    pub read_bps: f64,
    pub write_bps: f64,
    /// popup only, from the PhysicalDisk counters: busy share 0..1, queue length, average ms per
    /// transfer, transfers per second
    pub active: Option<f32>,
    pub queue: Option<f32>,
    pub latency_ms: Option<f32>,
    pub iops: Option<f32>,
    /// counted in the widget and the chart (Physical disks list in the popup toggles it)
    pub enabled: bool,
}

fn bus_name(bus: u32) -> &'static str {
    match bus {
        1 => "SCSI",
        2 => "ATAPI",
        3 => "ATA",
        4 => "IEEE 1394",
        5 => "SSA",
        6 => "Fibre",
        7 => "USB",
        8 => "RAID",
        9 => "iSCSI",
        10 => "SAS",
        11 => "SATA",
        12 => "SD",
        13 => "MMC",
        14 => "Virtual",
        15 => "FileBackedVirtual",
        16 => "Spaces",
        17 => "NVMe",
        18 => "SCM",
        19 => "UFS",
        _ => "",
    }
}

fn cstr_at(buf: &[u8], off: u32) -> String {
    let off = off as usize;
    if off == 0 || off >= buf.len() {
        return String::new();
    }
    let end = buf[off..].iter().position(|b| *b == 0).map(|p| off + p).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[off..end]).trim().to_string()
}

fn open(index: u32) -> Option<HANDLE> {
    let path = wide(&format!("\\\\.\\PhysicalDrive{index}"));
    unsafe { CreateFileW(pcwstr(&path), 0, FILE_SHARE_READ | FILE_SHARE_WRITE, None, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, None).ok() }
}

/// One IOCTL_STORAGE_QUERY_PROPERTY standard query into `buf`; the bytes returned.
fn property(h: HANDLE, id: STORAGE_PROPERTY_ID, buf: &mut [u8]) -> usize {
    let q = STORAGE_PROPERTY_QUERY { PropertyId: id, QueryType: PropertyStandardQuery, AdditionalParameters: [0] };
    let mut got = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            h,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some(&q as *const _ as *const _),
            std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            Some(buf.as_mut_ptr() as *mut _),
            buf.len() as u32,
            Some(&mut got),
            None,
        )
        .is_ok()
    };
    if ok { got as usize } else { 0 }
}

/// A one-byte BOOLEAN after the Version / Size header (seek penalty, TRIM descriptors).
fn flag(h: HANDLE, id: STORAGE_PROPERTY_ID) -> Option<bool> {
    let mut buf = [0u8; 16];
    (property(h, id, &mut buf) >= 9).then(|| buf[8] != 0)
}

/// Current temperature of disk `index` in °C: the first sensor of the temperature property.
pub fn temperature(index: u32) -> Option<f32> {
    let h = open(index)?;
    let mut buf = [0u8; 256];
    let got = property(h, StorageDeviceTemperatureProperty, &mut buf);
    unsafe {
        let _ = CloseHandle(h);
    }
    let head = std::mem::size_of::<STORAGE_TEMPERATURE_DATA_DESCRIPTOR>() - std::mem::size_of::<STORAGE_TEMPERATURE_INFO>();
    if got < head + std::mem::size_of::<STORAGE_TEMPERATURE_INFO>() {
        return None;
    }
    let d = unsafe { &*(buf.as_ptr() as *const STORAGE_TEMPERATURE_DATA_DESCRIPTOR) };
    if d.InfoCount == 0 {
        return None;
    }
    let c = d.TemperatureInfo[0].Temperature;
    (c > 0 && c < 120).then_some(c as f32)
}

fn query(index: u32) -> Option<PhysicalDisk> {
    let h = open(index)?;
    let result = unsafe {
        let q = STORAGE_PROPERTY_QUERY { PropertyId: StorageDeviceProperty, QueryType: PropertyStandardQuery, AdditionalParameters: [0] };
        let mut buf = vec![0u8; 4096];
        let mut got = 0u32;
        let ok = DeviceIoControl(
            h,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some(&q as *const _ as *const _),
            std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            Some(buf.as_mut_ptr() as *mut _),
            buf.len() as u32,
            Some(&mut got),
            None,
        )
        .is_ok();
        if !ok || (got as usize) < std::mem::size_of::<STORAGE_DEVICE_DESCRIPTOR>() {
            None
        } else {
            let d = &*(buf.as_ptr() as *const STORAGE_DEVICE_DESCRIPTOR);
            let vendor = cstr_at(&buf, d.VendorIdOffset);
            let product = cstr_at(&buf, d.ProductIdOffset);
            let model = if vendor.is_empty() || product.starts_with(&vendor) { product } else { format!("{vendor} {product}") };
            let firmware = cstr_at(&buf, d.ProductRevisionOffset);
            // some drivers pad the serial with spaces or dots, a few return it hex-encoded
            let serial = clean_serial(&cstr_at(&buf, d.SerialNumberOffset));
            // IOCTL_DISK_GET_LENGTH_INFO needs read access (admin); the geometry query does not
            let mut geo = DISK_GEOMETRY_EX::default();
            let mut got2 = 0u32;
            let size = if DeviceIoControl(
                h,
                IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
                None,
                0,
                Some(&mut geo as *mut _ as *mut _),
                std::mem::size_of::<DISK_GEOMETRY_EX>() as u32,
                Some(&mut got2),
                None,
            )
            .is_ok()
            {
                geo.DiskSize.max(0) as u64
            } else {
                0
            };
            // mounted VHD / Dev Drive images are not hardware
            let virtual_disk = matches!(d.BusType.0, 14 | 15);
            if virtual_disk {
                None
            } else {
                Some(PhysicalDisk {
                    index,
                    model: if model.is_empty() { format!("Disk {index}") } else { model },
                    bus: bus_name(d.BusType.0 as u32),
                    size,
                    serial,
                    firmware,
                    ssd: flag(h, StorageDeviceSeekPenaltyProperty).map(|penalty| !penalty),
                    trim: flag(h, StorageDeviceTrimProperty),
                    temp: None,
                    read_bps: 0.0,
                    write_bps: 0.0,
                    active: None,
                    queue: None,
                    latency_ms: None,
                    iops: None,
                    enabled: true,
                })
            }
        }
    };
    unsafe {
        let _ = CloseHandle(h);
    }
    result
}

/// Apply the user's switched-off list; when it would leave nothing on, everything stays on.
pub fn with_disabled(mut disks: Vec<PhysicalDisk>, disabled: &[String]) -> Vec<PhysicalDisk> {
    for d in &mut disks {
        d.enabled = !disabled.contains(&d.model);
    }
    if !disks.iter().any(|d| d.enabled) {
        for d in &mut disks {
            d.enabled = true;
        }
    }
    disks
}

/// All physical drives 0..32 that answer (indices can have gaps after hot-unplugs).
pub fn enumerate() -> Vec<PhysicalDisk> {
    (0..32u32).filter_map(query).collect()
}

/// Serial as the drive reports it, without padding. Older storage drivers (Windows 7 AHCI)
/// hand out the ATA serial as hex text of its byte-swapped characters: "32534a42..." for
/// "S2BJ...". That is decoded back when it is clearly hex and decodes to printable text.
fn clean_serial(raw: &str) -> String {
    let t = raw.trim_matches(|c: char| c == '.' || c.is_whitespace());
    if t.len() >= 16 && t.len() % 4 == 0 && t.chars().all(|c| c.is_ascii_hexdigit()) {
        let bytes: Option<Vec<u8>> = (0..t.len()).step_by(2).map(|i| u8::from_str_radix(&t[i..i + 2], 16).ok()).collect();
        if let Some(mut b) = bytes {
            for pair in b.chunks_mut(2) {
                pair.swap(0, 1);
            }
            if b.iter().all(|c| (0x20..0x7F).contains(c)) {
                return String::from_utf8_lossy(&b).trim().to_string();
            }
        }
    }
    t.to_string()
}

#[cfg(test)]
mod serial_tests {
    use super::clean_serial;

    #[test]
    fn hex_serials_decode() {
        assert_eq!(clean_serial("2020202057202d44435752413157323035363032"), "WD-WCARW1026520");
        assert_eq!(clean_serial("  50026B77844A4BED  "), "50026B77844A4BED");
        assert_eq!(clean_serial("S4500105...."), "S4500105");
    }
}
