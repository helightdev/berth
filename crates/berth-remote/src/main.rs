//! berth-remote: the hosts of a berth serve, in this terminal.

use berth_remote::options::{self, Parsed};
use berth_remote::{device, settings};

fn main() {
    let options = match options::parse(std::env::args().skip(1), |name| std::env::var(name).ok()) {
        Ok(Parsed::Run(options)) => options,
        Ok(Parsed::Version) => {
            println!("berth-remote {}", berth_api::VERSION);
            return;
        }
        Ok(Parsed::Help) => {
            println!("{}", options::USAGE);
            return;
        }
        Err(error) => {
            eprintln!("berth-remote: {error}\n\n{}", options::USAGE);
            std::process::exit(2);
        }
    };
    let settings = settings::Settings::new(settings::folder(|name| std::env::var(name).ok()));
    match device::Terminal::new(options.linked, settings, options.forget) {
        Ok(terminal) => berth_page::device::install(terminal),
        Err(error) => {
            eprintln!("berth-remote: could not forget the kept sign-in: {error}");
            std::process::exit(1);
        }
    }
    if let Err(error) = berth_remote::screens::run(None) {
        eprintln!("berth-remote: {error}");
        std::process::exit(1);
    }
}
