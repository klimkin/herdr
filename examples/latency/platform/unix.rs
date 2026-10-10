pub fn raw_stdin() -> std::io::Result<()> {
    let mut settings = std::mem::MaybeUninit::<libc::termios>::uninit();
    if unsafe { libc::tcgetattr(libc::STDIN_FILENO, settings.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut settings = unsafe { settings.assume_init() };
    unsafe { libc::cfmakeraw(&mut settings) };
    if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &settings) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
