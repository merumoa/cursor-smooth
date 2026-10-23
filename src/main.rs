use cursor_smooth::cli::{self, Command};
use cursor_smooth::platform;

fn main() -> std::process::ExitCode {
    let command = match cli::parse(std::env::args().skip(1)) {
        Ok(command) => command,
        Err(error) => {
            eprintln!("cursor-smooth: {}", error.message());
            eprintln!("Try 'cursor-smooth --help'.");
            return std::process::ExitCode::from(platform::EXIT_USAGE as u8);
        }
    };

    let code = match command {
        Command::Help => {
            print!("{}", cli::USAGE);
            platform::EXIT_OK
        }
        Command::Version => {
            println!("cursor-smooth {}", cursor_smooth::VERSION);
            platform::EXIT_OK
        }
        Command::Doctor => platform::doctor(),
        Command::Run(options) => {
            println!(
                "cursor-smooth {}: run (1000 Hz output opportunities, deliberate buffering)",
                cursor_smooth::VERSION
            );
            platform::run(&options)
        }
    };

    std::process::ExitCode::from(code.clamp(0, 255) as u8)
}
