use std::io;
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

pub fn monotonic_ns() -> io::Result<u64> {
    let mut frequency = 0;
    let mut counter = 0;
    if unsafe { QueryPerformanceFrequency(&mut frequency) } == 0
        || unsafe { QueryPerformanceCounter(&mut counter) } == 0
        || frequency <= 0
        || counter < 0
    {
        return Err(io::Error::last_os_error());
    }
    u64::try_from((counter as u128) * 1_000_000_000 / (frequency as u128))
        .map_err(|_| io::Error::other("monotonic timestamp overflow"))
}
