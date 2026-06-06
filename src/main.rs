use std::collections::HashMap;
use std::fmt::Write as FmtWrite;
use std::fs;
use std::io::BufRead;
use std::sync::mpsc;
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;

// Raw sample types

struct Sample {
    at: Instant,
    cpu: CpuTicks,
    load: f32,
    mem: MemSample,
    net: Vec<IfaceSample>,
    disk: Vec<DiskDevSample>,
    procs: Vec<ProcSample>,
    gpus: Vec<GpuSample>,
}

#[derive(Default)]
struct CpuTicks {
    total: u64,
    idle: u64,
}

struct ProcSample {
    pid: u32,
    cpu_ticks: u64,
    rss_bytes: u64,
}

#[derive(Default)]
struct MemSample {
    total_bytes: u64,
    available_bytes: u64,
    swap_total_bytes: u64,
    swap_free_bytes: u64,
}

struct GpuSample {
    card: String,
    busy: u8,
    temp_mc: i32,
    power_mw: u32,
    vram_used: u64,
    vram_total: u64,
    procs: Vec<GpuProcSample>,
}

#[derive(Clone)]
struct GpuProcSample {
    pid: u32,
    vram_kib: u64,
    engine_gfx_ns: u64,
}

struct IfaceSample {
    name: String,
    rx_bytes: u64,
    tx_bytes: u64,
}

struct DiskDevSample {
    name: String,
    read_bytes: u64,
    write_bytes: u64,
}

enum Vendor {
    Amd,
    Nvidia,
    Intel,
}

// NVML FFI types, layout verified against nvml.h
const NVML_SUCCESS: u32 = 0;

type FnNvmlInit = unsafe extern "C" fn() -> u32;
type FnNvmlGetHandleByPciBusId =
    unsafe extern "C" fn(*const libc::c_char, *mut *mut libc::c_void) -> u32;
type FnNvmlGetUtilizationRates =
    unsafe extern "C" fn(*mut libc::c_void, *mut NvmlUtilization) -> u32;
type FnNvmlGetMemoryInfo = unsafe extern "C" fn(*mut libc::c_void, *mut NvmlMemory) -> u32;

#[repr(C)]
struct NvmlUtilization {
    gpu: u32,
    memory: u32,
}

#[repr(C)]
struct NvmlMemory {
    total: u64,
    free: u64,
    used: u64,
}

struct NvmlLib {
    _handle: *mut libc::c_void,
    get_handle_by_pci: FnNvmlGetHandleByPciBusId,
    get_utilization: FnNvmlGetUtilizationRates,
    get_memory_info: FnNvmlGetMemoryInfo,
}

// Static GPU metadata, discovered once at startup, cards (probably) don't change at runtime.
struct GpuCardMeta {
    card: String,
    pdev: String,
    vendor: Vendor,
    path_busy: String,
    path_vram_used: String,
    path_vram_total: String,
    hwmon_path: Option<String>,
    nvml_device: Option<*mut libc::c_void>,
}

// What the caller wants sampled this tick
struct Request {
    cpu: bool,
    mem: bool,
    gpu: bool,
    net: bool,
    disk: bool,
    procs: bool,
    // true = exclude procs with 0 cpu/mem/vram usage
    filter_zero: bool,
    // None = unlimited
    proc_limit: Option<usize>,
}

impl Default for Request {
    fn default() -> Self {
        Self {
            cpu: true,
            mem: true,
            gpu: true,
            net: true,
            disk: true,
            procs: true,
            filter_zero: false,
            proc_limit: None,
        }
    }
}

impl Request {
    fn parse(line: &str) -> Self {
        let mut r = Self {
            cpu: false,
            mem: false,
            gpu: false,
            net: false,
            disk: false,
            procs: false,
            filter_zero: false,
            proc_limit: None,
        };
        let mut tokens = line.split_whitespace().peekable();
        while let Some(token) = tokens.next() {
            match token {
                "cpu" => r.cpu = true,
                "mem" => r.mem = true,
                "gpu" => r.gpu = true,
                "net" => r.net = true,
                "disk" => r.disk = true,
                "procs" => r.procs = true,
                "nozero" => r.filter_zero = true,
                "limit" => {
                    if let Some(&next) = tokens.peek() {
                        if let Ok(n) = next.parse::<usize>() {
                            tokens.next();
                            r.proc_limit = if n == 0 { None } else { Some(n) };
                        }
                    }
                }
                _ => {}
            }
        }
        r
    }
}

// JSON output types

#[derive(Serialize)]
struct Output {
    #[serde(skip_serializing_if = "Option::is_none")]
    cpu: Option<CpuOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    memory: Option<MemOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    gpu: Option<Vec<GpuOut>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    net: Option<Vec<IfaceOut>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    disk: Option<Vec<DiskOut>>,
}

#[derive(Serialize)]
struct CpuOut {
    percent: f32,
    load: f32,
    procs: Vec<ProcCpuOut>,
}

#[derive(Serialize)]
struct ProcCpuOut {
    name: String,
    pid: u32,
    cpu: f32,
}

#[derive(Serialize)]
struct MemOut {
    used_bytes: u64,
    total_bytes: u64,
    swap_used_bytes: u64,
    swap_total_bytes: u64,
    procs: Vec<ProcMemOut>,
}

#[derive(Serialize)]
struct ProcMemOut {
    name: String,
    pid: u32,
    rss: u64,
}

#[derive(Serialize)]
struct GpuOut {
    card: String,
    busy: u8,
    temp_c: f32,
    power_w: f32,
    vram_used: u64,
    vram_total: u64,
    procs: Vec<GpuProcOut>,
}

#[derive(Serialize)]
struct GpuProcOut {
    name: String,
    pid: u32,
    vram_kib: u64,
    gfx_pct: f32,
}

#[derive(Serialize)]
struct IfaceOut {
    name: String,
    rx_bytes_per_sec: u64,
    tx_bytes_per_sec: u64,
}

#[derive(Serialize)]
struct DiskOut {
    name: String,
    read_bytes_per_sec: u64,
    write_bytes_per_sec: u64,
    partitions: Vec<DiskOut>,
}

fn sample(cards: &[GpuCardMeta], req: &Request, nvml: Option<&NvmlLib>) -> Sample {
    let at = Instant::now();
    let cpu = if req.cpu {
        read_cpu_ticks()
    } else {
        CpuTicks::default()
    };
    let load = if req.cpu { read_load() } else { 0.0 };
    let mem = if req.mem {
        read_mem()
    } else {
        MemSample::default()
    };

    // Pass empty card slice when GPU procs aren't needed so walk_proc skips fdinfo.
    let walk_cards: &[GpuCardMeta] = if req.gpu { cards } else { &[] };
    let (procs, gpu_proc_map) = if req.procs {
        walk_proc(walk_cards)
    } else {
        (Vec::new(), HashMap::new())
    };

    let gpus = if req.gpu {
        cards
            .iter()
            .map(|meta| {
                let (busy, temp_mc, power_mw, vram_used, vram_total) = match meta.vendor {
                    Vendor::Amd => read_card_stats_amd(meta),
                    Vendor::Nvidia => read_card_stats_nvidia(meta, nvml),
                    Vendor::Intel => read_card_stats_intel(meta),
                };
                let gpu_procs = gpu_proc_map.get(&meta.pdev).cloned().unwrap_or_default();
                GpuSample {
                    card: meta.card.clone(),
                    busy,
                    temp_mc,
                    power_mw,
                    vram_used,
                    vram_total,
                    procs: gpu_procs,
                }
            })
            .collect()
    } else {
        Vec::new()
    };

    let net = if req.net { read_net() } else { Vec::new() };
    let disk = if req.disk { read_disk() } else { Vec::new() };

    Sample {
        at,
        cpu,
        load,
        mem,
        net,
        disk,
        procs,
        gpus,
    }
}

fn read_cpu_ticks() -> CpuTicks {
    let s = fs::read_to_string("/proc/stat").unwrap_or_default();
    let line = s.lines().next().unwrap_or_default();
    let ns: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .map(|v| v.parse().unwrap_or(0))
        .collect();
    let idle = ns.get(3).copied().unwrap_or(0) + ns.get(4).copied().unwrap_or(0);
    let total = ns.iter().sum();
    CpuTicks { total, idle }
}

fn read_load() -> f32 {
    fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| s.split_whitespace().next().and_then(|v| v.parse().ok()))
        .unwrap_or(0.0)
}

fn read_mem() -> MemSample {
    let s = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let mut total = 0u64;
    let mut available = 0u64;
    let mut swap_total = 0u64;
    let mut swap_free = 0u64;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total = parse_kb(rest);
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            available = parse_kb(rest);
        } else if let Some(rest) = line.strip_prefix("SwapTotal:") {
            swap_total = parse_kb(rest);
        } else if let Some(rest) = line.strip_prefix("SwapFree:") {
            swap_free = parse_kb(rest);
        }
    }
    MemSample {
        total_bytes: total,
        available_bytes: available,
        swap_total_bytes: swap_total,
        swap_free_bytes: swap_free,
    }
}

fn page_size() -> u64 {
    static PAGE_SIZE: OnceLock<u64> = OnceLock::new();
    // sysconf(_SC_PAGESIZE) has no preconditions and never fails
    *PAGE_SIZE.get_or_init(|| unsafe { libc::sysconf(libc::_SC_PAGESIZE) as u64 })
}

fn parse_kb(s: &str) -> u64 {
    s.split_whitespace()
        .next()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0)
        * 1024
}

fn detect_vendor(dev: &str) -> Option<Vendor> {
    match fs::read_to_string(format!("{dev}/vendor"))
        .unwrap_or_default()
        .trim()
    {
        "0x1002" => Some(Vendor::Amd),
        "0x10de" => Some(Vendor::Nvidia),
        "0x8086" => Some(Vendor::Intel),
        _ => None,
    }
}

fn nvml_load() -> Option<NvmlLib> {
    unsafe {
        let handle = libc::dlopen(
            b"libnvidia-ml.so.1\0".as_ptr() as *const libc::c_char,
            libc::RTLD_LAZY,
        );
        if handle.is_null() {
            return None;
        }

        let init_ptr = libc::dlsym(handle, b"nvmlInit_v2\0".as_ptr() as _);
        let pci_ptr = libc::dlsym(handle, b"nvmlDeviceGetHandleByPciBusId_v2\0".as_ptr() as _);
        let util_ptr = libc::dlsym(handle, b"nvmlDeviceGetUtilizationRates\0".as_ptr() as _);
        let mem_ptr = libc::dlsym(handle, b"nvmlDeviceGetMemoryInfo\0".as_ptr() as _);

        if init_ptr.is_null() || pci_ptr.is_null() || util_ptr.is_null() || mem_ptr.is_null() {
            libc::dlclose(handle);
            return None;
        }

        let init_fn: FnNvmlInit = std::mem::transmute(init_ptr);
        if init_fn() != NVML_SUCCESS {
            libc::dlclose(handle);
            return None;
        }

        Some(NvmlLib {
            _handle: handle,
            get_handle_by_pci: std::mem::transmute(pci_ptr),
            get_utilization: std::mem::transmute(util_ptr),
            get_memory_info: std::mem::transmute(mem_ptr),
        })
    }
}

fn nvml_device_for_pdev(nvml: &NvmlLib, pdev: &str) -> Option<*mut libc::c_void> {
    let Ok(pci) = std::ffi::CString::new(pdev) else {
        return None;
    };
    let mut device: *mut libc::c_void = std::ptr::null_mut();
    let ret = unsafe { (nvml.get_handle_by_pci)(pci.as_ptr(), &mut device) };
    if ret != NVML_SUCCESS || device.is_null() {
        None
    } else {
        Some(device)
    }
}

fn discover_gpu_cards(nvml: Option<&NvmlLib>) -> Vec<GpuCardMeta> {
    let mut cards = Vec::new();
    let Ok(dir) = fs::read_dir("/sys/class/drm") else {
        return cards;
    };
    for entry in dir.flatten() {
        let card = entry.file_name().to_string_lossy().to_string();
        if !card.starts_with("card") || card.contains('-') {
            continue;
        }
        let dev = format!("/sys/class/drm/{card}/device");
        let Some(vendor) = detect_vendor(&dev) else {
            continue;
        };
        let pdev = read_pdev(&dev);
        let path_busy = format!("{dev}/gpu_busy_percent");
        let path_vram_used = format!("{dev}/mem_info_vram_used");
        let path_vram_total = format!("{dev}/mem_info_vram_total");
        let hwmon_path = fs::read_dir(format!("{dev}/hwmon"))
            .ok()
            .and_then(|mut d| d.next())
            .and_then(|e| e.ok())
            .map(|e| e.path().to_string_lossy().into_owned());
        let nvml_device = if matches!(vendor, Vendor::Nvidia) {
            nvml.and_then(|n| nvml_device_for_pdev(n, &pdev))
        } else {
            None
        };
        cards.push(GpuCardMeta {
            card,
            pdev,
            vendor,
            path_busy,
            path_vram_used,
            path_vram_total,
            hwmon_path,
            nvml_device,
        });
    }
    cards.sort_by(|a, b| a.card.cmp(&b.card));
    cards
}

fn read_net() -> Vec<IfaceSample> {
    let s = fs::read_to_string("/proc/net/dev").unwrap_or_default();
    let mut ifaces = Vec::new();
    for line in s.lines().skip(2) {
        let Some((name, rest)) = line.trim().split_once(':') else {
            continue;
        };
        let name = name.trim().to_string();
        if name == "lo" {
            continue;
        }
        let f: Vec<&str> = rest.split_whitespace().collect();
        let rx_bytes = f.first().and_then(|v| v.parse().ok()).unwrap_or(0);
        let tx_bytes = f.get(8).and_then(|v| v.parse().ok()).unwrap_or(0);
        ifaces.push(IfaceSample {
            name,
            rx_bytes,
            tx_bytes,
        });
    }
    ifaces
}

fn read_disk() -> Vec<DiskDevSample> {
    let s = fs::read_to_string("/proc/diskstats").unwrap_or_default();
    let mut devices = Vec::new();
    for line in s.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        let Some(&name) = f.get(2) else { continue };
        if name.starts_with("loop") || name.starts_with("ram") {
            continue;
        }
        let read_bytes = f.get(5).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0) * 512;
        let write_bytes = f.get(9).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0) * 512;
        devices.push(DiskDevSample {
            name: name.to_string(),
            read_bytes,
            write_bytes,
        });
    }
    devices
}

fn read_sysfs_u64(path: impl AsRef<std::path::Path>) -> u64 {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn read_gpu_hwmon(meta: &GpuCardMeta) -> (i32, u32) {
    let Some(ref hwmon) = meta.hwmon_path else {
        return (0, 0);
    };
    let path = std::path::Path::new(hwmon);
    let temp = fs::read_to_string(path.join("temp1_input"))
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .unwrap_or(0);
    let power_mw = fs::read_to_string(path.join("power1_average"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|uw| (uw / 1000) as u32)
        .unwrap_or(0);
    (temp, power_mw)
}

fn read_card_stats_amd(meta: &GpuCardMeta) -> (u8, i32, u32, u64, u64) {
    let busy = read_sysfs_u64(&meta.path_busy) as u8;
    let vram_used = read_sysfs_u64(&meta.path_vram_used);
    let vram_total = read_sysfs_u64(&meta.path_vram_total);
    let (temp_mc, power_mw) = read_gpu_hwmon(meta);
    (busy, temp_mc, power_mw, vram_used, vram_total)
}

fn read_card_stats_nvidia(meta: &GpuCardMeta, nvml: Option<&NvmlLib>) -> (u8, i32, u32, u64, u64) {
    let (temp_mc, power_mw) = read_gpu_hwmon(meta);
    let (Some(nvml), Some(device)) = (nvml, meta.nvml_device) else {
        return (0, temp_mc, power_mw, 0, 0);
    };
    let mut util = NvmlUtilization { gpu: 0, memory: 0 };
    let mut mem = NvmlMemory {
        total: 0,
        free: 0,
        used: 0,
    };
    unsafe {
        if (nvml.get_utilization)(device, &mut util) != NVML_SUCCESS {
            util.gpu = 0;
        }
        if (nvml.get_memory_info)(device, &mut mem) != NVML_SUCCESS {
            mem.used = 0;
            mem.total = 0;
        }
    }
    (util.gpu as u8, temp_mc, power_mw, mem.used, mem.total)
}

fn read_card_stats_intel(meta: &GpuCardMeta) -> (u8, i32, u32, u64, u64) {
    // busy% not available via sysfs; VRAM paths exist for xe discrete GPUs, zero for iGPU
    let vram_used = read_sysfs_u64(&meta.path_vram_used);
    let vram_total = read_sysfs_u64(&meta.path_vram_total);
    let (temp_mc, power_mw) = read_gpu_hwmon(meta);
    (0, temp_mc, power_mw, vram_used, vram_total)
}

fn read_pdev(dev: &str) -> String {
    fs::read_to_string(format!("{dev}/uevent"))
        .unwrap_or_default()
        .lines()
        .find(|l| l.starts_with("PCI_SLOT_NAME="))
        .and_then(|l| l.split('=').nth(1))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

// Read argv[0] from cmdline, basename it, strip NixOS wrapper suffixes.
fn read_proc_name(pid: u32) -> Option<String> {
    let cmdline = fs::read_to_string(format!("/proc/{pid}/cmdline")).ok()?;
    let arg0 = cmdline.split('\0').next()?.trim();
    if arg0.is_empty() {
        return None;
    }
    let base = arg0.rsplit('/').next().unwrap_or(arg0);
    let clean = base
        .strip_prefix('.')
        .unwrap_or(base)
        .trim_end_matches("-wrapped")
        .trim_end_matches("-wrapper")
        .trim_end_matches("-wrap");
    if clean.is_empty() {
        return None;
    }
    Some(clean.to_string())
}

// Read cmdline name, fall back to comm from /proc/{pid}/comm.
fn resolve_name(pid: u32) -> String {
    read_proc_name(pid)
        .or_else(|| {
            fs::read_to_string(format!("/proc/{pid}/comm"))
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| pid.to_string())
}

fn read_proc_stat(pid: u32, path: &mut String) -> Option<(u64, u64)> {
    path.clear();
    write!(path, "/proc/{pid}/stat").unwrap();
    let s = fs::read_to_string(&*path).ok()?;
    // Skip past the comm field "(name)", rfind handles names containing '('
    let end = s.rfind(')')?;
    let after = s.get(end + 2..)?;
    // Fields after state: state(0) ppid(1) pgrp(2) session(3) tty_nr(4) tpgid(5) flags(6)
    // minflt(7) cminflt(8) majflt(9) cmajflt(10) utime(11) stime(12) ... rss(21)
    let mut iter = after.split_ascii_whitespace();
    let utime: u64 = iter.nth(11)?.parse().ok()?;
    let stime: u64 = iter.next()?.parse().ok()?;
    let rss_pages: u64 = iter.nth(8)?.parse().ok()?;
    Some((utime + stime, rss_pages * page_size()))
}

fn parse_fdinfo(vendor: &Vendor, content: &str) -> Option<(u64, u64)> {
    match vendor {
        Vendor::Amd => parse_fdinfo_amd(content),
        Vendor::Nvidia => parse_fdinfo_nvidia(content),
        Vendor::Intel => parse_fdinfo_intel(content),
    }
}

fn fdinfo_u64(content: &str, key: &str) -> u64 {
    content
        .lines()
        .find(|l| l.starts_with(key))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn parse_fdinfo_amd(content: &str) -> Option<(u64, u64)> {
    if !content
        .lines()
        .any(|l| l.starts_with("drm-driver:") && l.contains("amdgpu"))
    {
        return None;
    }
    Some((
        fdinfo_u64(content, "drm-memory-vram:"),
        fdinfo_u64(content, "drm-engine-gfx:"),
    ))
}

fn parse_fdinfo_nvidia(content: &str) -> Option<(u64, u64)> {
    if !content
        .lines()
        .any(|l| l.starts_with("drm-driver:") && l.contains("nvidia"))
    {
        return None;
    }
    let vram = fdinfo_u64(content, "drm-memory-device:");
    // prefer graphics engine time, fall back to compute
    let ns = content
        .lines()
        .find(|l| l.starts_with("drm-engine-graphics:") || l.starts_with("drm-engine-compute:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    Some((vram, ns))
}

fn parse_fdinfo_intel(content: &str) -> Option<(u64, u64)> {
    if !content
        .lines()
        .any(|l| l.starts_with("drm-driver:") && (l.contains("i915") || l.contains("xe")))
    {
        return None;
    }
    // prefer device memory (Arc dGPU), fall back to system memory (iGPU)
    let vram = if fdinfo_u64(content, "drm-memory-device:") > 0 {
        fdinfo_u64(content, "drm-memory-device:")
    } else {
        fdinfo_u64(content, "drm-memory-system:")
    };
    Some((vram, fdinfo_u64(content, "drm-engine-render:")))
}

// Walk /proc once, collecting process stats and per-GPU fdinfo simultaneously.
fn walk_proc(cards: &[GpuCardMeta]) -> (Vec<ProcSample>, HashMap<String, Vec<GpuProcSample>>) {
    let mut procs = Vec::with_capacity(512);
    let card_by_pdev: HashMap<&str, &GpuCardMeta> =
        cards.iter().map(|c| (c.pdev.as_str(), c)).collect();
    // pid -> (max_vram_kib, sum_engine_gfx_ns) per pdev
    let mut gpu_raw: HashMap<&str, HashMap<u32, (u64, u64)>> = cards
        .iter()
        .map(|c| (c.pdev.as_str(), HashMap::new()))
        .collect();

    let Ok(dir) = fs::read_dir("/proc") else {
        return (procs, Default::default());
    };
    let mut path = String::with_capacity(32);
    for entry in dir.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };

        let Some((cpu_ticks, rss_bytes)) = read_proc_stat(pid, &mut path) else {
            continue;
        };
        procs.push(ProcSample {
            pid,
            cpu_ticks,
            rss_bytes,
        });

        if cards.is_empty() {
            continue;
        }
        // Skip fdinfo entirely if the process has no open /dev/dri/ fds.
        path.clear();
        write!(path, "/proc/{pid}/fd").unwrap();
        let has_dri = fs::read_dir(&path).ok().is_some_and(|fds| {
            fds.flatten().any(|fd| {
                fs::read_link(fd.path())
                    .ok()
                    .and_then(|p| p.to_str().map(|s| s.starts_with("/dev/dri/")))
                    .unwrap_or(false)
            })
        });
        if !has_dri {
            continue;
        }
        path.clear();
        write!(path, "/proc/{pid}/fdinfo").unwrap();
        let Ok(fds) = fs::read_dir(&path) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(content) = fs::read_to_string(fd.path()) else {
                continue;
            };
            let fd_pdev = content
                .lines()
                .find(|l| l.starts_with("drm-pdev:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or_default();
            let Some(card) = card_by_pdev.get(fd_pdev) else {
                continue;
            };
            let Some((vram, ns)) = parse_fdinfo(&card.vendor, &content) else {
                continue;
            };
            let Some(by_pid) = gpu_raw.get_mut(fd_pdev) else {
                continue;
            };
            let e = by_pid.entry(pid).or_insert((0, 0));
            e.0 = e.0.max(vram);
            e.1 += ns;
        }
    }

    let gpu_procs = gpu_raw
        .into_iter()
        .map(|(pdev, by_pid)| {
            let samples = by_pid
                .into_iter()
                .map(|(pid, (vk, ns))| GpuProcSample {
                    pid,
                    vram_kib: vk,
                    engine_gfx_ns: ns,
                })
                .collect();
            (pdev.to_string(), samples)
        })
        .collect();

    (procs, gpu_procs)
}

fn bytes_per_sec(delta: u64, elapsed_ns: u64) -> u64 {
    if elapsed_ns > 0 {
        delta * 1_000_000_000 / elapsed_ns
    } else {
        0
    }
}

fn diff_cpu_procs(
    curr: &[ProcSample],
    prev_ticks: &HashMap<u32, u64>,
    total_d: u64,
    filter_zero: bool,
    proc_limit: Option<usize>,
) -> Vec<ProcCpuOut> {
    let mut procs: Vec<ProcCpuOut> = curr
        .iter()
        .filter_map(|p| {
            let dt = p
                .cpu_ticks
                .saturating_sub(*prev_ticks.get(&p.pid).unwrap_or(&p.cpu_ticks));
            if filter_zero && dt == 0 {
                return None;
            }
            Some(ProcCpuOut {
                name: String::new(),
                pid: p.pid,
                cpu: if total_d > 0 {
                    dt as f32 / total_d as f32 * 100.0
                } else {
                    0.0
                },
            })
        })
        .collect();
    procs.sort_by(|a, b| {
        b.cpu
            .partial_cmp(&a.cpu)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    if let Some(limit) = proc_limit {
        procs.truncate(limit);
    }
    for p in &mut procs {
        p.name = resolve_name(p.pid);
    }
    procs
}

fn diff_mem_procs(
    curr: &[ProcSample],
    filter_zero: bool,
    proc_limit: Option<usize>,
) -> Vec<ProcMemOut> {
    let mut procs: Vec<ProcMemOut> = curr
        .iter()
        .filter(|p| !filter_zero || p.rss_bytes > 0)
        .map(|p| ProcMemOut {
            name: String::new(),
            pid: p.pid,
            rss: p.rss_bytes,
        })
        .collect();
    procs.sort_by(|a, b| b.rss.cmp(&a.rss));
    if let Some(limit) = proc_limit {
        procs.truncate(limit);
    }
    for p in &mut procs {
        p.name = resolve_name(p.pid);
    }
    procs
}

fn diff_gpu_procs(
    g: &GpuSample,
    prev_ns: &HashMap<u32, u64>,
    elapsed_ns: u64,
    filter_zero: bool,
    proc_limit: Option<usize>,
) -> Vec<GpuProcOut> {
    let mut procs: Vec<GpuProcOut> = g
        .procs
        .iter()
        .filter(|p| !filter_zero || p.vram_kib > 0)
        .map(|p| {
            let ns_d = p
                .engine_gfx_ns
                .saturating_sub(*prev_ns.get(&p.pid).unwrap_or(&p.engine_gfx_ns));
            GpuProcOut {
                name: String::new(),
                pid: p.pid,
                vram_kib: p.vram_kib,
                gfx_pct: if elapsed_ns > 0 {
                    ns_d as f32 / elapsed_ns as f32 * 100.0
                } else {
                    0.0
                },
            }
        })
        .collect();
    procs.sort_by(|a, b| b.vram_kib.cmp(&a.vram_kib));
    if let Some(limit) = proc_limit {
        procs.truncate(limit);
    }
    for p in &mut procs {
        p.name = resolve_name(p.pid);
    }
    procs
}

fn diff_gpu_card(
    g: &GpuSample,
    prev_gpus: &[GpuSample],
    elapsed_ns: u64,
    include_procs: bool,
    filter_zero: bool,
    proc_limit: Option<usize>,
) -> GpuOut {
    let prev_ns: HashMap<u32, u64> = prev_gpus
        .iter()
        .find(|pg| pg.card == g.card)
        .map(|pg| pg.procs.iter().map(|p| (p.pid, p.engine_gfx_ns)).collect())
        .unwrap_or_default();

    let gprocs = if include_procs {
        diff_gpu_procs(g, &prev_ns, elapsed_ns, filter_zero, proc_limit)
    } else {
        Vec::new()
    };

    GpuOut {
        card: g.card.clone(),
        busy: g.busy,
        temp_c: g.temp_mc as f32 / 1000.0,
        power_w: g.power_mw as f32 / 1000.0,
        vram_used: g.vram_used,
        vram_total: g.vram_total,
        procs: gprocs,
    }
}

fn diff(prev: &Sample, curr: &Sample, req: &Request) -> Output {
    let elapsed_ns = curr.at.duration_since(prev.at).as_nanos() as u64;

    let cpu = if req.cpu {
        let total_d = curr.cpu.total.saturating_sub(prev.cpu.total);
        let idle_d = curr.cpu.idle.saturating_sub(prev.cpu.idle);
        let cpu_pct = if total_d > 0 {
            (total_d - idle_d) as f32 / total_d as f32 * 100.0
        } else {
            0.0
        };

        let cpu_procs = if req.procs {
            let prev_ticks: HashMap<u32, u64> =
                prev.procs.iter().map(|p| (p.pid, p.cpu_ticks)).collect();
            diff_cpu_procs(
                &curr.procs,
                &prev_ticks,
                total_d,
                req.filter_zero,
                req.proc_limit,
            )
        } else {
            Vec::new()
        };

        Some(CpuOut {
            percent: cpu_pct,
            load: curr.load,
            procs: cpu_procs,
        })
    } else {
        None
    };

    let memory = if req.mem {
        let mem_procs = if req.procs {
            diff_mem_procs(&curr.procs, req.filter_zero, req.proc_limit)
        } else {
            Vec::new()
        };

        Some(MemOut {
            used_bytes: curr
                .mem
                .total_bytes
                .saturating_sub(curr.mem.available_bytes),
            total_bytes: curr.mem.total_bytes,
            swap_used_bytes: curr
                .mem
                .swap_total_bytes
                .saturating_sub(curr.mem.swap_free_bytes),
            swap_total_bytes: curr.mem.swap_total_bytes,
            procs: mem_procs,
        })
    } else {
        None
    };

    let gpu = if req.gpu {
        let gpu_out: Vec<GpuOut> = curr
            .gpus
            .iter()
            .map(|g| {
                diff_gpu_card(
                    g,
                    &prev.gpus,
                    elapsed_ns,
                    req.procs,
                    req.filter_zero,
                    req.proc_limit,
                )
            })
            .collect();
        Some(gpu_out)
    } else {
        None
    };

    let net = if req.net {
        let prev_net: HashMap<&str, &IfaceSample> =
            prev.net.iter().map(|i| (i.name.as_str(), i)).collect();
        let mut ifaces: Vec<IfaceOut> = curr
            .net
            .iter()
            .map(|i| {
                let (rx_d, tx_d) = prev_net.get(i.name.as_str()).map_or((0, 0), |p| {
                    (
                        i.rx_bytes.saturating_sub(p.rx_bytes),
                        i.tx_bytes.saturating_sub(p.tx_bytes),
                    )
                });
                IfaceOut {
                    name: i.name.clone(),
                    rx_bytes_per_sec: bytes_per_sec(rx_d, elapsed_ns),
                    tx_bytes_per_sec: bytes_per_sec(tx_d, elapsed_ns),
                }
            })
            .collect();
        ifaces.sort_by(|a, b| a.name.cmp(&b.name));
        Some(ifaces)
    } else {
        None
    };

    let disk = if req.disk {
        let prev_disk: HashMap<&str, &DiskDevSample> =
            prev.disk.iter().map(|d| (d.name.as_str(), d)).collect();

        // Compute rates for all devices into a map.
        let mut dev_map: HashMap<String, DiskOut> = curr
            .disk
            .iter()
            .map(|d| {
                let (rd, wd) = prev_disk.get(d.name.as_str()).map_or((0, 0), |p| {
                    (
                        d.read_bytes.saturating_sub(p.read_bytes),
                        d.write_bytes.saturating_sub(p.write_bytes),
                    )
                });
                (
                    d.name.clone(),
                    DiskOut {
                        name: d.name.clone(),
                        read_bytes_per_sec: bytes_per_sec(rd, elapsed_ns),
                        write_bytes_per_sec: bytes_per_sec(wd, elapsed_ns),
                        partitions: Vec::new(),
                    },
                )
            })
            .collect();

        // A device is a partition if another device's name is a strict prefix of it.
        // Use the longest matching prefix as the immediate parent.
        let names: Vec<String> = dev_map.keys().cloned().collect();
        let find_parent = |name: &str| -> Option<String> {
            names
                .iter()
                .filter(|n| n.as_str() != name && name.starts_with(n.as_str()))
                .max_by_key(|n| n.len())
                .cloned()
        };

        // Pull partitions out of the map and group by parent.
        let mut by_parent: HashMap<String, Vec<DiskOut>> = HashMap::new();
        for name in &names {
            if let Some(parent) = find_parent(name) {
                if let Some(dev) = dev_map.remove(name.as_str()) {
                    by_parent.entry(parent).or_default().push(dev);
                }
            }
        }

        // Attach partition lists to their parents.
        for (parent_name, mut parts) in by_parent {
            if let Some(parent) = dev_map.get_mut(&parent_name) {
                parts.sort_by(|a, b| a.name.cmp(&b.name));
                parent.partitions = parts;
            }
        }

        let mut devs: Vec<DiskOut> = dev_map.into_values().collect();
        devs.sort_by(|a, b| a.name.cmp(&b.name));
        Some(devs)
    } else {
        None
    };

    Output {
        cpu,
        memory,
        gpu,
        net,
        disk,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut interval_ms: u64 = args.first().and_then(|s| s.parse().ok()).unwrap_or(1000);
    let initial_req = if args.len() > 1 {
        args[1..].join(" ")
    } else {
        String::new()
    };

    // Background thread feeds stdin lines into a channel.
    let (tx, rx) = mpsc::channel::<String>();
    thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines().flatten() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let nvml = nvml_load();
    let cards = discover_gpu_cards(nvml.as_ref());
    let mut req = if initial_req.is_empty() {
        Request::default()
    } else {
        Request::parse(&initial_req)
    };
    let mut prev = sample(&cards, &req, nvml.as_ref());
    thread::sleep(Duration::from_millis(interval_ms));

    loop {
        // Drain channel, last line wins if multiple arrived during sleep.
        while let Ok(line) = rx.try_recv() {
            if let Some(rest) = line.strip_prefix("interval") {
                if let Ok(ms) = rest.trim().parse::<u64>() {
                    interval_ms = ms;
                }
            } else {
                req = Request::parse(&line);
            }
        }

        let tick_start = Instant::now();
        let curr = sample(&cards, &req, nvml.as_ref());
        println!(
            "{}",
            serde_json::to_string(&diff(&prev, &curr, &req)).unwrap()
        );
        prev = curr;
        let target = Duration::from_millis(interval_ms);
        let elapsed = tick_start.elapsed();
        if elapsed < target {
            thread::sleep(target - elapsed);
        }
    }
}
