pub fn raw_stdin() -> std::io::Result<()> {
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_VIRTUAL_TERMINAL_INPUT,
        STD_INPUT_HANDLE,
    };
    let input = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    let mut mode = 0;
    if unsafe { GetConsoleMode(input, &mut mode) } == 0
        || unsafe { SetConsoleMode(input, ENABLE_VIRTUAL_TERMINAL_INPUT) } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
