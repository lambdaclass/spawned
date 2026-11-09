//! Fallible Linux CPU placement. No affinity is applied to the calling thread
//! during validation; runtime owners pin themselves before admitting work.
use std::io;
#[cfg(target_os = "linux")]
pub fn available_cpus() -> io::Result<Vec<usize>> {
    let set =
        nix::sched::sched_getaffinity(nix::unistd::Pid::from_raw(0)).map_err(io::Error::from)?;
    (0..nix::libc::CPU_SETSIZE as usize)
        .filter_map(|cpu| match set.is_set(cpu) {
            Ok(true) => Some(Ok(cpu)),
            Ok(false) => None,
            Err(error) => Some(Err(io::Error::from(error))),
        })
        .collect()
}
#[cfg(target_os = "linux")]
pub fn pin_current(cpu: usize) -> io::Result<()> {
    let mut set = nix::sched::CpuSet::new();
    set.set(cpu).map_err(io::Error::from)?;
    nix::sched::sched_setaffinity(nix::unistd::Pid::from_raw(0), &set).map_err(io::Error::from)
}
#[cfg(target_os = "linux")]
pub fn current_cpu() -> io::Result<usize> {
    nix::sched::sched_getcpu().map_err(io::Error::from)
}
#[cfg(not(target_os = "linux"))]
pub fn available_cpus() -> io::Result<Vec<usize>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "hard CPU affinity requires Linux",
    ))
}
#[cfg(not(target_os = "linux"))]
pub fn pin_current(_: usize) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "hard CPU affinity requires Linux",
    ))
}
#[cfg(not(target_os = "linux"))]
pub fn current_cpu() -> io::Result<usize> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "CPU audit requires Linux",
    ))
}
