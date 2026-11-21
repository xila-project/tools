//! The `xila` executable: a thin wrapper around the `xila_tools` library.

image::instantiate_global_allocator!();

fn main() {
    let arguments = match std::env::args_os()
        .skip(1)
        .map(|argument| argument.into_string())
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(arguments) => arguments,
        Err(_) => {
            eprintln!("xila: arguments must be valid UTF-8");
            std::process::exit(2);
        }
    };
    if let Err(error) = xila_tools::run(arguments.iter().map(String::as_str)) {
        eprintln!("xila: {error}");
        std::process::exit(1);
    }
}
