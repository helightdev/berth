//! berth-remote: the hosts of a berth serve, in this terminal.

use berth_page::settings;
use berth_remote::options::{self, Parsed};

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
    let var = |name: &str| std::env::var(name).ok();
    let settings = match options.forget {
        true => settings::Settings::new(None),
        false => settings::Settings::new(settings::folder(var)),
    };
    berth_page::device::install(options.linked, settings);
    if let Err(error) = berth_page::screens::run(None) {
        eprintln!("berth-remote: {error}");
        std::process::exit(1);
    }
}
