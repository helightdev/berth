//! The image of a berth server, built from this berth rather than pulled.
//!
//! Its build context is a tar stream of three files: the Dockerfile CI builds
//! the published image from, `requirements.txt` of the provisioning source,
//! and this program as `berth`. A machine's own Docker engine builds it from
//! standard input, as `docker build -t berth:VERSION -`, so nothing is
//! unpacked there. Only a static Linux program for that machine's CPU goes
//! into it: one that loads the system's C library would not run on the
//! image's system.
//!
//! The tag is local, `berth:VERSION`, so a self-built image never passes for
//! a published one; its labels say where it came from and the repository a
//! release is pulled from again.

use std::io::Write;
use std::process::{Command, Stdio};

/// The Dockerfile of the server's image, as CI builds the published one.
pub const DOCKERFILE: &str = include_str!("../../../container/berth.Dockerfile");

/// Where the published images are, and where a self-built server goes back to.
pub const BASELINE: &str = "ghcr.io/helightdev/berth";

/// The repository of a self-built image's tag.
pub const REPOSITORY: &str = "berth";

/// The value of [`label::SOURCE`] on an image built from a berth.
pub const SELF_BUILT: &str = "self-built";

/// The labels the Dockerfile sets.
pub mod label {
    pub const VERSION: &str = "org.opencontainers.image.version";
    /// `self-built` or `published`.
    pub const SOURCE: &str = "dev.berth.source";
    /// The repository a release is pulled from.
    pub const BASELINE: &str = "dev.berth.baseline";
    /// The SHA-256 of the program in it.
    pub const BINARY: &str = "dev.berth.binary-sha256";
}

/// The CPUs berth is released for on Linux.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cpu {
    Amd64,
    Arm64,
}

impl Cpu {
    /// The CPU `uname -m` names, or Docker's name for it.
    pub fn named(machine: &str) -> Option<Cpu> {
        match machine.trim() {
            "x86_64" | "amd64" => Some(Cpu::Amd64),
            "aarch64" | "arm64" => Some(Cpu::Arm64),
            _ => None,
        }
    }

    /// The CPU of this machine.
    pub fn here() -> Option<Cpu> {
        Cpu::named(std::env::consts::ARCH)
    }

    pub fn name(self) -> &'static str {
        match self {
            Cpu::Amd64 => "amd64",
            Cpu::Arm64 => "arm64",
        }
    }

    fn elf_machine(self) -> u16 {
        match self {
            Cpu::Amd64 => 62,
            Cpu::Arm64 => 183,
        }
    }

    fn of_elf(machine: u16) -> Option<Cpu> {
        [Cpu::Amd64, Cpu::Arm64]
            .into_iter()
            .find(|cpu| cpu.elf_machine() == machine)
    }

    /// The release's file of berth for this CPU.
    pub fn release_file(self) -> &'static str {
        match self {
            Cpu::Amd64 => "berth-x86_64-linux.tar.gz",
            Cpu::Arm64 => "berth-aarch64-linux.tar.gz",
        }
    }
}

/// What to use in place of a program that cannot go into the image.
pub fn release_hint(cpu: Cpu) -> String {
    let version = berth_api::VERSION;
    format!(
        "Run this from berth {version} for Linux, from {} of https://github.com/helightdev/berth/releases/tag/v{version}, or from a static build: cargo build --release --target {}-unknown-linux-musl -p berth",
        cpu.release_file(),
        match cpu {
            Cpu::Amd64 => "x86_64",
            Cpu::Arm64 => "aarch64",
        }
    )
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(bytes: &[u8], at: usize) -> Option<usize> {
    let value = u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?);
    usize::try_from(value).ok()
}

/// Why `program` cannot run in the image on a machine with `cpu`, if it
/// cannot: it has to be a 64-bit Linux program for that CPU that names no
/// interpreter, which a static build does not.
pub fn unfit(program: &[u8], cpu: Cpu) -> Option<String> {
    inspect(program, cpu).err()
}

fn inspect(program: &[u8], cpu: Cpu) -> Result<(), String> {
    const PT_INTERP: u32 = 3;
    let short = || "This berth's program header is cut short".to_owned();
    if program.get(..4) != Some(b"\x7fELF") {
        let what = match program.get(..4) {
            Some([0xcf, 0xfa, 0xed, 0xfe] | [0xca, 0xfe, 0xba, 0xbe]) => "a program for macOS",
            Some([b'M', b'Z', ..]) => "a program for Windows",
            _ => "not a Linux program",
        };
        return Err(format!("This berth is {what}"));
    }
    if program.get(4) != Some(&2) || program.get(5) != Some(&1) {
        return Err("This berth is not a 64-bit little-endian Linux program".into());
    }
    let machine = u16_at(program, 18).ok_or_else(short)?;
    match Cpu::of_elf(machine) {
        Some(built) if built != cpu => {
            return Err(format!(
                "This berth is built for {}, and the machine runs {}",
                built.name(),
                cpu.name()
            ));
        }
        None => {
            return Err(format!(
                "This berth is built for a CPU berth is not released for (ELF machine {machine})"
            ));
        }
        Some(_) => {}
    }
    let table = u64_at(program, 32).ok_or_else(short)?;
    let size = usize::from(u16_at(program, 54).ok_or_else(short)?);
    let count = usize::from(u16_at(program, 56).ok_or_else(short)?);
    for header in (0..count).map(|index| table.saturating_add(index * size)) {
        if u32_at(program, header).ok_or_else(short)? != PT_INTERP {
            continue;
        }
        let offset = u64_at(program, header + 8).ok_or_else(short)?;
        let length = u64_at(program, header + 32).ok_or_else(short)?;
        let interpreter = program
            .get(offset..offset.saturating_add(length))
            .map(|name| {
                String::from_utf8_lossy(name)
                    .trim_end_matches('\0')
                    .to_owned()
            })
            .unwrap_or_default();
        return Err(format!(
            "This berth loads the system's C library through {interpreter}, which the image's system may not have: it is not a static build"
        ));
    }
    Ok(())
}

pub fn sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// One file of a tar stream: a ustar header, then its bytes to a whole block.
fn tar_entry(out: &mut Vec<u8>, name: &str, mode: u32, bytes: &[u8]) {
    let mut header = [0u8; 512];
    let mut put = |at: usize, text: &[u8]| header[at..at + text.len()].copy_from_slice(text);
    put(0, name.as_bytes());
    put(100, format!("{mode:07o}\0").as_bytes());
    put(108, b"0000000\0");
    put(116, b"0000000\0");
    put(124, format!("{:011o}\0", bytes.len()).as_bytes());
    put(136, b"00000000000\0");
    put(148, b"        ");
    put(156, b"0");
    put(257, b"ustar\0");
    put(263, b"00");
    let sum: u32 = header.iter().map(|&byte| u32::from(byte)).sum();
    header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    out.extend_from_slice(&header);
    out.extend_from_slice(bytes);
    out.resize(out.len().div_ceil(512) * 512, 0);
}

/// The build context of the image: the Dockerfile, the provisioning
/// source's `requirements.txt` and the program, as a tar stream.
pub fn tar(program: &[u8], requirements: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(program.len() + 16 * 1024);
    tar_entry(&mut out, "Dockerfile", 0o644, DOCKERFILE.as_bytes());
    tar_entry(&mut out, "requirements.txt", 0o644, requirements);
    tar_entry(&mut out, "berth", 0o755, program);
    out.resize(out.len() + 1024, 0);
    out
}

/// The stream compressed with gzip, which `docker build -` reads as it is,
/// or as it was when this machine has no gzip.
pub fn compressed(tar: Vec<u8>) -> Vec<u8> {
    let Ok(mut gzip) = Command::new("gzip")
        .arg("-c")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return tar;
    };
    let mut input = gzip.stdin.take().expect("piped");
    let writer = std::thread::spawn(move || {
        let _ = input.write_all(&tar);
        tar
    });
    let output = gzip.wait_with_output();
    let tar = writer.join().expect("the writer of gzip's input");
    match output {
        Ok(output) if output.status.success() && !output.stdout.is_empty() => output.stdout,
        _ => tar,
    }
}

/// The image's build context, and what the build is told of it.
pub struct Context {
    /// The context, compressed.
    pub bytes: Vec<u8>,
    /// The SHA-256 of the program in it.
    pub sha256: String,
}

impl Context {
    /// The context of `program`, after checking that it fits a machine with `cpu`.
    pub fn of(program: &[u8], requirements: &[u8], cpu: Cpu) -> Result<Context, String> {
        if let Some(why) = unfit(program, cpu) {
            return Err(format!("{why}. {}", release_hint(cpu)));
        }
        Ok(Context {
            sha256: sha256(program),
            bytes: compressed(tar(program, requirements)),
        })
    }

    /// The context of this berth, for a machine with `cpu`. Its
    /// `requirements.txt` is the provisioning source's in `source`.
    pub fn of_this_berth(source: &std::path::Path, cpu: Cpu) -> Result<Context, String> {
        let file = source.join("provision/requirements.txt");
        let requirements =
            std::fs::read(&file).map_err(|error| format!("{}: {error}", file.display()))?;
        let program = std::fs::read("/proc/self/exe")
            .map_err(|error| format!("This berth's own program cannot be read: {error}"))?;
        Context::of(&program, &requirements, cpu)
    }

    /// The `--build-arg` options of `docker build` for it.
    pub fn build_args(&self) -> Vec<String> {
        [
            format!("BERTH_VERSION={}", berth_api::VERSION),
            format!("BERTH_SHA256={}", self.sha256),
        ]
        .into_iter()
        .flat_map(|arg| ["--build-arg".to_owned(), arg])
        .collect()
    }
}

/// The local tag of the image of this berth.
pub fn tag() -> String {
    format!("{REPOSITORY}:{}", berth_api::VERSION)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The files of a tar stream: name, mode and bytes.
    fn entries(tar: &[u8]) -> Vec<(String, u32, Vec<u8>)> {
        let mut found = vec![];
        let mut at = 0;
        while at + 512 <= tar.len() && tar[at] != 0 {
            let header = &tar[at..at + 512];
            let text = |range: std::ops::Range<usize>| {
                String::from_utf8_lossy(&header[range])
                    .trim_end_matches(['\0', ' '])
                    .to_owned()
            };
            let size = usize::from_str_radix(&text(124..136), 8).unwrap();
            let mode = u32::from_str_radix(&text(100..108), 8).unwrap();
            // The checksum is that of the header with its own field as spaces.
            let mut blank = header.to_vec();
            blank[148..156].fill(b' ');
            let sum: u32 = blank.iter().map(|&byte| u32::from(byte)).sum();
            assert_eq!(u32::from_str_radix(&text(148..154), 8).unwrap(), sum);
            found.push((text(0..100), mode, tar[at + 512..at + 512 + size].to_vec()));
            at += 512 + size.div_ceil(512) * 512;
        }
        found
    }

    /// A 64-bit little-endian ELF header for `machine`, with one program
    /// header of `kind` that points at `interpreter`.
    fn elf(machine: u16, kind: u32, interpreter: &str) -> Vec<u8> {
        let mut program = vec![0u8; 256];
        program[..6].copy_from_slice(b"\x7fELF\x02\x01");
        program[18..20].copy_from_slice(&machine.to_le_bytes());
        program[32..40].copy_from_slice(&64u64.to_le_bytes());
        program[54..56].copy_from_slice(&56u16.to_le_bytes());
        program[56..58].copy_from_slice(&1u16.to_le_bytes());
        program[64..68].copy_from_slice(&kind.to_le_bytes());
        program[72..80].copy_from_slice(&160u64.to_le_bytes());
        program[96..104].copy_from_slice(&(interpreter.len() as u64 + 1).to_le_bytes());
        program[160..160 + interpreter.len()].copy_from_slice(interpreter.as_bytes());
        program
    }

    #[test]
    fn only_a_static_linux_program_for_the_machines_cpu_fits() {
        const LOAD: u32 = 1;
        const INTERP: u32 = 3;
        assert_eq!(unfit(&elf(62, LOAD, ""), Cpu::Amd64), None);
        assert_eq!(unfit(&elf(183, LOAD, ""), Cpu::Arm64), None);
        let other = unfit(&elf(183, LOAD, ""), Cpu::Amd64).unwrap();
        assert!(
            other.contains("built for arm64, and the machine runs amd64"),
            "{other}"
        );
        let glibc = unfit(&elf(62, INTERP, "/lib64/ld-linux-x86-64.so.2"), Cpu::Amd64).unwrap();
        assert!(glibc.contains("/lib64/ld-linux-x86-64.so.2"), "{glibc}");
        assert!(glibc.contains("not a static build"), "{glibc}");
        let macos = unfit(&[0xcf, 0xfa, 0xed, 0xfe, 7, 0, 0, 1], Cpu::Amd64).unwrap();
        assert!(macos.contains("a program for macOS"), "{macos}");
        assert!(
            unfit(b"MZ\x90\x00", Cpu::Amd64)
                .unwrap()
                .contains("Windows")
        );
        assert!(unfit(b"#!/bin/sh\n", Cpu::Amd64).is_some());
        // A header cut short is refused rather than read past.
        assert!(unfit(&elf(62, LOAD, "")[..40], Cpu::Amd64).is_some());
    }

    #[test]
    fn a_refusal_names_the_release_to_use() {
        let refused = Context::of(&elf(62, 3, "/lib/ld.so"), b"", Cpu::Arm64)
            .err()
            .unwrap();
        assert!(refused.contains(Cpu::Arm64.release_file()), "{refused}");
        assert!(refused.contains(berth_api::VERSION), "{refused}");
        assert_eq!(Cpu::named("x86_64"), Some(Cpu::Amd64));
        assert_eq!(Cpu::named("aarch64\n"), Some(Cpu::Arm64));
        assert_eq!(Cpu::named("riscv64"), None);
    }

    #[test]
    fn the_context_holds_the_dockerfile_the_requirements_and_the_program() {
        let program = elf(62, 1, "");
        let tar = tar(&program, b"ansible-core==2.21.6\n");
        assert_eq!(tar.len() % 512, 0);
        let found = entries(&tar);
        let names: Vec<&str> = found.iter().map(|(name, ..)| name.as_str()).collect();
        assert_eq!(names, ["Dockerfile", "requirements.txt", "berth"]);
        assert_eq!(found[0].2, DOCKERFILE.as_bytes());
        assert_eq!(found[1].2, b"ansible-core==2.21.6\n");
        assert_eq!((found[2].1, &found[2].2), (0o755, &program));
        let context = Context::of(&program, b"x\n", Cpu::Amd64).unwrap();
        assert_eq!(context.sha256, sha256(&program));
        assert_eq!(context.sha256.len(), 64);
        let args = context.build_args().join(" ");
        assert!(args.contains(&format!("--build-arg BERTH_VERSION={}", berth_api::VERSION)));
        assert!(args.contains(&format!("--build-arg BERTH_SHA256={}", context.sha256)));
        assert_eq!(tag(), format!("berth:{}", berth_api::VERSION));
    }

    #[test]
    fn the_compressed_context_is_the_same_stream() {
        let tar = tar(&elf(62, 1, ""), b"x\n");
        let packed = compressed(tar.clone());
        if packed == tar {
            return; // This machine has no gzip.
        }
        let mut gunzip = Command::new("gzip")
            .arg("-dc")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        gunzip.stdin.take().unwrap().write_all(&packed).unwrap();
        assert_eq!(gunzip.wait_with_output().unwrap().stdout, tar);
    }

    /// The file CI builds the published image from, by the workflow.
    #[test]
    fn the_dockerfile_berth_carries_is_the_one_ci_builds() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let workflow = std::fs::read_to_string(root.join(".github/workflows/release.yml")).unwrap();
        let image = workflow
            .split("\n  image:\n")
            .nth(1)
            .expect("the workflow has a job image");
        let file = image
            .lines()
            .find_map(|line| line.trim().strip_prefix("file: "))
            .expect("the image job names its Dockerfile");
        let ci = std::fs::read_to_string(root.join(file)).unwrap();
        assert_eq!(
            DOCKERFILE, ci,
            "berth carries another Dockerfile than the one CI builds, {file}"
        );
        // It labels the image as berth's helper reads it.
        for label in [
            label::VERSION,
            label::SOURCE,
            label::BASELINE,
            label::BINARY,
        ] {
            assert!(DOCKERFILE.contains(label), "{label}");
        }
        assert!(DOCKERFILE.contains(&format!("BERTH_BASELINE={BASELINE}")));
        assert!(image.contains("BERTH_SOURCE=published"));
    }
}
