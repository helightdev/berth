use berthd::{Mode, State};
use std::{
    io::{Read, Write},
    path::PathBuf,
    process::ExitCode,
};

const USAGE: &str = "berthd: the API of a berth host.

Usage:
  berthd                     run as the system service: configuration in /etc/berth,
                             sockets in /run/berth, state in /var/lib/berth
  berthd --single DIR        run as yourself with everything under DIR (sockets,
                             state, and configuration in DIR/etc when that exists,
                             else /etc/berth); for containers and tests
  berthd connect [--dir DIR] join standard input and output to the control socket
  berthd openapi | asyncapi  print the API documents
  berthd --version";

/// Copy standard input and output to the control socket until either side closes.
fn connect(dir: PathBuf) -> ExitCode {
    let path = dir.join(berth_api::CONTROL_SOCKET);
    let stream = match std::os::unix::net::UnixStream::connect(&path) {
        Ok(stream) => stream,
        Err(error) => {
            eprintln!("berthd: {}: {error}", path.display());
            return ExitCode::from(1);
        }
    };
    let mut to_socket = stream.try_clone().expect("a socket can be cloned");
    std::thread::spawn(move || {
        let mut buffer = [0; 16384];
        let mut input = std::io::stdin().lock();
        while let Ok(count) = input.read(&mut buffer)
            && count > 0
            && to_socket.write_all(&buffer[..count]).is_ok()
        {}
        // No more requests will come; what is still being answered gets out.
        let _ = to_socket.shutdown(std::net::Shutdown::Write);
    });
    let mut from_socket = stream;
    let mut output = std::io::stdout().lock();
    let mut buffer = [0; 16384];
    while let Ok(count) = from_socket.read(&mut buffer)
        && count > 0
        && output
            .write_all(&buffer[..count])
            .and_then(|()| output.flush())
            .is_ok()
    {}
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let run_dir =
        || PathBuf::from(std::env::var("BERTH_DIR").unwrap_or_else(|_| berth_api::RUN_DIR.into()));
    let (mode, run, config, state) = match words.as_slice() {
        [] => (
            Mode::System,
            PathBuf::from(berth_api::RUN_DIR),
            PathBuf::from("/etc/berth"),
            PathBuf::from("/var/lib/berth"),
        ),
        ["--single", dir] => {
            let dir = PathBuf::from(dir);
            let own = dir.join("etc");
            let config = if own.is_dir() {
                own
            } else {
                PathBuf::from("/etc/berth")
            };
            (Mode::Single, dir.clone(), config, dir.join("state"))
        }
        ["connect"] => return connect(run_dir()),
        ["connect", "--dir", dir] => return connect(PathBuf::from(dir)),
        ["files-helper", ..] => return ExitCode::from(berthd::helper_main(&args[1..]) as u8),
        ["openapi"] => {
            println!(
                "{}",
                serde_json::to_string_pretty(&berthd::openapi()).expect("a document")
            );
            return ExitCode::SUCCESS;
        }
        ["operations"] => {
            println!(
                "{}",
                serde_json::to_string_pretty(&berthd::operations()).expect("a document")
            );
            return ExitCode::SUCCESS;
        }
        ["asyncapi"] => {
            println!(
                "{}",
                serde_json::to_string_pretty(&berthd::asyncapi()).expect("a document")
            );
            return ExitCode::SUCCESS;
        }
        ["--version"] => {
            println!("berthd {} (api {})", berth_api::VERSION, berth_api::API);
            return ExitCode::SUCCESS;
        }
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let outcome = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .and_then(|runtime| {
            runtime.block_on(async {
                let state = State::new(mode, config, state)?;
                berthd::serve(state, &run).await
            })
        });
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("berthd: {error}");
            ExitCode::from(1)
        }
    }
}
