//! Memory and CPU of the browser's processes, read from the kernel
//! (`proc_pid_rusage`, as Activity Monitor does), for Settings → Tabs and the
//! heavy-tab badge. Nothing is sampled until one of them asks.

use std::ffi::{c_int, c_void};

/// The first fields of `rusage_info_v4` (sys/resource.h).
#[repr(C)]
struct RusageInfoV4 {
    uuid: [u8; 16],
    user_time: u64,
    system_time: u64,
    pkg_idle_wkups: u64,
    interrupt_wkups: u64,
    pageins: u64,
    wired_size: u64,
    resident_size: u64,
    phys_footprint: u64,
    proc_start_abstime: u64,
    _rest: [u64; 26],
}

const RUSAGE_INFO_V4: c_int = 4;

unsafe extern "C" {
    fn proc_listallpids(buf: *mut c_void, size: c_int) -> c_int;
    fn proc_pid_rusage(pid: c_int, flavor: c_int, buf: *mut RusageInfoV4) -> c_int;
    fn proc_name(pid: c_int, buf: *mut c_void, size: u32) -> c_int;
    // Private but stable (Activity Monitor uses it): the app a process is charged to.
    fn responsibility_get_pid_responsible_for_pid(pid: c_int) -> c_int;
    fn mach_timebase_info(info: *mut [u32; 2]) -> c_int;
}

/// One process: memory as macOS counts it ("Memory" in Activity Monitor),
/// CPU time used so far, and wakeups so far.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    pub footprint: u64,
    pub cpu_ns: u64,
    pub wakeups: u64,
}

fn rusage(pid: i32) -> Option<RusageInfoV4> {
    let mut info: RusageInfoV4 = unsafe { std::mem::zeroed() };
    (unsafe { proc_pid_rusage(pid, RUSAGE_INFO_V4, &mut info) } == 0).then_some(info)
}

fn abs_to_ns(t: u64) -> u64 {
    let mut tb = [0u32; 2];
    unsafe { mach_timebase_info(&mut tb) };
    t * tb[0] as u64 / tb[1].max(1) as u64
}

pub fn of(pid: i32) -> Option<Usage> {
    rusage(pid).map(|i| Usage {
        footprint: i.phys_footprint,
        cpu_ns: abs_to_ns(i.user_time + i.system_time),
        wakeups: i.pkg_idle_wkups + i.interrupt_wkups,
    })
}

fn name(pid: i32) -> String {
    let mut buf = [0u8; 64];
    let n = unsafe { proc_name(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    String::from_utf8_lossy(&buf[..n.max(0) as usize]).into_owned()
}

/// WebKit's helper processes working for this app (pages, network,
/// graphics). They are started by launchd, not by us, so they aren't our
/// children; they are charged to us and started after us.
pub fn webkit_helpers() -> Vec<i32> {
    let me = std::process::id() as i32;
    let (Some(mine), responsible) = (rusage(me), unsafe { responsibility_get_pid_responsible_for_pid(me) }) else {
        return Vec::new();
    };
    let mut pids = vec![0i32; 8192];
    let n = unsafe { proc_listallpids(pids.as_mut_ptr().cast(), (pids.len() * 4) as c_int) };
    pids.truncate(n.max(0) as usize);
    pids.into_iter()
        .filter(|&pid| pid != me)
        .filter(|&pid| rusage(pid).is_some_and(|i| i.proc_start_abstime >= mine.proc_start_abstime))
        .filter(|&pid| name(pid).starts_with("com.apple.WebKit"))
        .filter(|&pid| unsafe { responsibility_get_pid_responsible_for_pid(pid) } == responsible)
        .collect()
}

/// Why a background tab counts as heavy, from its process now and at the
/// previous sample: a lot of memory, or busy CPU while nobody looks at it.
pub fn heavy(now: Usage, before: Option<(Usage, f64)>) -> Option<String> {
    const MB: u64 = 1024 * 1024;
    let memory = (now.footprint >= 500 * MB).then(|| format!("Uses {} MB of memory", now.footprint / MB));
    let cpu = before.and_then(|(b, secs)| {
        let percent = now.cpu_ns.saturating_sub(b.cpu_ns) as f64 / 1e9 / secs.max(1.0) * 100.0;
        (percent >= 10.0).then(|| format!("Uses {percent:.0}% CPU in the background"))
    });
    match (memory, cpu) {
        (Some(m), Some(c)) => Some(format!("{m}. {c}")),
        (m, c) => m.or(c),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_this_process() {
        assert_eq!(std::mem::size_of::<RusageInfoV4>(), 16 + 35 * 8, "rusage_info_v4 layout");
        let u = of(std::process::id() as i32).unwrap();
        assert!(u.footprint > 0);
    }

    #[test]
    fn heavy_tabs_use_lots_of_memory_or_cpu() {
        let mb = 1024 * 1024;
        let light = Usage { footprint: 80 * mb, cpu_ns: 1_000_000_000, wakeups: 0 };
        assert_eq!(heavy(light, None), None);
        let big = Usage { footprint: 900 * mb, ..light };
        assert_eq!(heavy(big, None).as_deref(), Some("Uses 900 MB of memory"));
        // 6 s of CPU over 30 s: 20%.
        let busy = Usage { cpu_ns: 7_000_000_000, ..light };
        assert_eq!(heavy(busy, Some((light, 30.0))).as_deref(), Some("Uses 20% CPU in the background"));
        assert_eq!(heavy(Usage { cpu_ns: 1_500_000_000, ..light }, Some((light, 30.0))), None, "idle enough");
    }
}
