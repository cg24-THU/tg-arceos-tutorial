use clap::{Parser, Subcommand};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{self, Command};

/// ArceOS sysmap multi-architecture build & run tool
#[derive(Parser)]
#[command(
    name = "xtask",
    about = "Build and run arceos-sysmap on different architectures"
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build the kernel for a given architecture
    Build {
        /// Target architecture: riscv64, aarch64, x86_64, loongarch64
        #[arg(long, default_value = "riscv64")]
        arch: String,
    },
    /// Build and run the kernel in QEMU
    Run {
        /// Target architecture: riscv64, aarch64, x86_64, loongarch64
        #[arg(long, default_value = "riscv64")]
        arch: String,
    },
}

#[allow(dead_code)]
struct ArchInfo {
    target: &'static str,
    platform: &'static str,
    objcopy_arch: &'static str,
    musl_prefix: &'static str,
}

fn arch_info(arch: &str) -> ArchInfo {
    match arch {
        "riscv64" => ArchInfo {
            target: "riscv64gc-unknown-none-elf",
            platform: "riscv64-qemu-virt",
            objcopy_arch: "riscv64",
            musl_prefix: "riscv64-linux-musl",
        },
        "aarch64" => ArchInfo {
            target: "aarch64-unknown-none-softfloat",
            platform: "aarch64-qemu-virt",
            objcopy_arch: "aarch64",
            musl_prefix: "aarch64-linux-musl",
        },
        "x86_64" => ArchInfo {
            target: "x86_64-unknown-none",
            platform: "x86-pc",
            objcopy_arch: "x86_64",
            musl_prefix: "x86_64-linux-musl",
        },
        "loongarch64" => ArchInfo {
            target: "loongarch64-unknown-none",
            platform: "loongarch64-qemu-virt",
            objcopy_arch: "loongarch64",
            musl_prefix: "loongarch64-linux-musl",
        },
        _ => {
            eprintln!(
                "Error: unsupported architecture '{}'. \
                 Supported: riscv64, aarch64, x86_64, loongarch64",
                arch
            );
            process::exit(1);
        }
    }
}

/// Locate the project root.
fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Copy the architecture-specific axconfig to .axconfig.toml
fn install_config(root: &Path, arch: &str) {
    let src = root.join("configs").join(format!("{arch}.toml"));
    let dst = root.join(".axconfig.toml");
    if !src.exists() {
        eprintln!("Error: config file not found: {}", src.display());
        process::exit(1);
    }
    std::fs::copy(&src, &dst).unwrap_or_else(|e| {
        eprintln!(
            "Error: failed to copy {} -> {}: {}",
            src.display(),
            dst.display(),
            e
        );
        process::exit(1);
    });
    println!("Installed config: {} -> .axconfig.toml", src.display());
}

/// Find a cross-compilation tool by trying several prefixes in order.
/// Tries PATH first, then known fallback locations.
fn find_tool(prefixes: &[&str], tool: &str) -> String {
    for prefix in prefixes {
        let name = format!("{prefix}-{tool}");

        // Try PATH first
        if let Ok(output) = Command::new("which").arg(&name).output() {
            if output.status.success() {
                let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !path.is_empty() {
                    return path;
                }
            }
        }

        // Try known fallback locations
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home".into());
        let fallback_dirs = [
            format!("{home}/thecodes/{prefix}-cross/bin"),
            format!("/opt/{prefix}-cross/bin"),
            format!("/usr/local/bin"),
            String::from("/usr/bin"),
        ];
        for dir in &fallback_dirs {
            let path = format!("{dir}/{name}");
            if Path::new(&path).exists() {
                return path;
            }
        }
    }

    // Last resort: just return the first candidate and hope it's in PATH.
    format!("{}-{tool}", prefixes[0])
}

/// Build the user-space payload (`mapfile` from `payload/mapfile_c/mapfile.c`).
/// Mirrors `payload/mapfile_c/Makefile` (static musl-gcc + strip) per target triple.
fn build_payload(root: &Path, info: &ArchInfo) -> PathBuf {
    const MAPFILE_BIN: &str = "mapfile";

    let mapfile_c_dir = root.join("payload").join("mapfile_c");
    let mapfile_c = mapfile_c_dir.join("mapfile.c");
    if !mapfile_c.exists() {
        eprintln!(
            "Error: mapfile source not found: {} (expected exercise mapfile payload)",
            mapfile_c.display()
        );
        process::exit(1);
    }

    let out_dir = mapfile_c_dir.join("target").join(info.musl_prefix);
    std::fs::create_dir_all(&out_dir).unwrap();
    let mapfile_elf = out_dir.join(MAPFILE_BIN);

    let gnu_prefix = info.musl_prefix.replace("-musl", "-gnu");
    let prefixes = [info.musl_prefix, gnu_prefix.as_str()];
    let gcc = find_tool(&prefixes, "gcc");
    let strip = find_tool(&prefixes, "strip");

    println!("Building mapfile payload with {} ...", gcc);

    let status = Command::new(&gcc)
        .args([
            "-static",
            mapfile_c.to_str().unwrap(),
            "-o",
            mapfile_elf.to_str().unwrap(),
        ])
        .status()
        .unwrap_or_else(|e| {
            eprintln!("Error: failed to run {}: {}", gcc, e);
            process::exit(1);
        });
    if !status.success() {
        eprintln!("Error: mapfile payload compilation failed");
        process::exit(status.code().unwrap_or(1));
    }

    let status = Command::new(&strip)
        .arg(mapfile_elf.to_str().unwrap())
        .status()
        .unwrap_or_else(|e| {
            eprintln!("Error: failed to run {}: {}", strip, e);
            process::exit(1);
        });
    if !status.success() {
        eprintln!("Warning: strip failed, continuing with unstripped binary");
    }

    println!("Payload built: {}", mapfile_elf.display());
    mapfile_elf
}

/// Create a 64MB FAT32 disk image containing `/sbin/mapfile` (see `load_user_app` in the kernel).
fn create_fat_disk_image(path: &Path, payload_elf: &Path) {
    const DISK_SIZE: u64 = 64 * 1024 * 1024;

    // Read the payload binary
    let payload_data = std::fs::read(payload_elf).unwrap_or_else(|e| {
        eprintln!(
            "Error: failed to read payload {}: {}",
            payload_elf.display(),
            e
        );
        process::exit(1);
    });
    println!("Payload binary size: {} bytes", payload_data.len());

    // Create or truncate the image file
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .unwrap_or_else(|e| {
            eprintln!("Error: failed to create disk image: {}", e);
            process::exit(1);
        });
    file.set_len(DISK_SIZE).unwrap();

    // Format as FAT32
    let format_opts = fatfs::FormatVolumeOptions::new().fat_type(fatfs::FatType::Fat32);
    fatfs::format_volume(&file, format_opts).unwrap_or_else(|e| {
        eprintln!("Error: failed to format FAT32: {}", e);
        process::exit(1);
    });

    // Populate filesystem
    {
        let fs = fatfs::FileSystem::new(&file, fatfs::FsOptions::new()).unwrap_or_else(|e| {
            eprintln!("Error: failed to open FAT filesystem: {}", e);
            process::exit(1);
        });
        let root_dir = fs.root_dir();

        // Create /sbin directory
        root_dir.create_dir("sbin").unwrap_or_else(|e| {
            eprintln!("Error: failed to create /sbin: {}", e);
            process::exit(1);
        });

        let mut f = root_dir.create_file("sbin/mapfile").unwrap_or_else(|e| {
            eprintln!("Error: failed to create /sbin/mapfile: {}", e);
            process::exit(1);
        });
        f.write_all(&payload_data).unwrap();
        f.flush().unwrap();
    }

    println!(
        "Created FAT32 disk image: {} ({}MB) with /sbin/mapfile",
        path.display(),
        DISK_SIZE / (1024 * 1024)
    );
}

/// Run cargo build for the target architecture.
fn do_build(root: &Path, info: &ArchInfo) {
    let manifest = root.join("Cargo.toml");
    let ax_config = root.join(".axconfig.toml");
    let status = Command::new("cargo")
        .args([
            "build",
            "--release",
            "--target",
            info.target,
            "--features",
            "axstd",
            "--manifest-path",
            manifest.to_str().unwrap(),
        ])
        // Ensure dependencies read the intended config regardless of subprocess cwd.
        .env("AX_CONFIG_PATH", ax_config.to_str().unwrap())
        .status()
        .expect("failed to execute cargo build");
    if !status.success() {
        eprintln!("Error: cargo build failed");
        process::exit(status.code().unwrap_or(1));
    }
}

/// Convert ELF to raw binary using rust-objcopy.
fn do_objcopy(elf: &Path, bin: &Path, objcopy_arch: &str) {
    let status = Command::new("rust-objcopy")
        .args([
            &format!("--binary-architecture={objcopy_arch}"),
            elf.to_str().unwrap(),
            "--strip-all",
            "-O",
            "binary",
            bin.to_str().unwrap(),
        ])
        .status()
        .expect("failed to execute rust-objcopy (install with: cargo install cargo-binutils)");
    if !status.success() {
        eprintln!("Error: rust-objcopy failed");
        process::exit(status.code().unwrap_or(1));
    }
}

/// Run QEMU with VirtIO block device.
fn do_run_qemu(arch: &str, elf: &Path, bin: &Path, disk: &Path) {
    let mem = "128M";
    let smp = "1";

    let qemu = format!("qemu-system-{arch}");

    let mut args: Vec<String> = vec![
        "-m".into(),
        mem.into(),
        "-smp".into(),
        smp.into(),
        "-nographic".into(),
    ];

    match arch {
        "riscv64" => {
            args.extend([
                "-machine".into(),
                "virt".into(),
                "-bios".into(),
                "default".into(),
                "-kernel".into(),
                bin.to_str().unwrap().into(),
            ]);
        }
        "aarch64" => {
            args.extend([
                "-cpu".into(),
                "cortex-a72".into(),
                "-machine".into(),
                "virt".into(),
                "-kernel".into(),
                bin.to_str().unwrap().into(),
            ]);
        }
        "x86_64" => {
            // x86_64 uses ELF directly, no objcopy needed
            args.extend([
                "-machine".into(),
                "q35".into(),
                "-kernel".into(),
                elf.to_str().unwrap().into(),
            ]);
        }
        "loongarch64" => {
            args.extend([
                "-machine".into(),
                "virt".into(),
                "-kernel".into(),
                bin.to_str().unwrap().into(),
            ]);
        }
        _ => unreachable!(),
    }

    // VirtIO block device
    args.extend([
        "-drive".into(),
        format!("file={},format=raw,if=none,id=disk0", disk.display()),
        "-device".into(),
        "virtio-blk-pci,drive=disk0".into(),
    ]);

    println!("Running: {} {}", qemu, args.join(" "));
    let status = Command::new(&qemu)
        .args(&args)
        .status()
        .unwrap_or_else(|e| {
            eprintln!("Error: failed to run {}: {}", qemu, e);
            process::exit(1);
        });
    if !status.success() {
        process::exit(status.code().unwrap_or(1));
    }
}

fn main() {
    let cli = Cli::parse();

    let root = project_root();

    match cli.command {
        Cmd::Build { ref arch } => {
            let info = arch_info(arch);
            install_config(&root, arch);
            let _payload = build_payload(&root, &info);
            do_build(&root, &info);
            println!("Build complete for {arch} ({})", info.target);
        }
        Cmd::Run { ref arch } => {
            let info = arch_info(arch);
            install_config(&root, arch);

            // 1. Build payload (equivalent to `make payload`)
            let payload_elf = build_payload(&root, &info);

            // 2. Create disk image with payload (equivalent to `./update_disk.sh`)
            let disk = root.join("target").join(format!("disk-{arch}.img"));
            create_fat_disk_image(&disk, &payload_elf);

            // 3. Build kernel (equivalent to `make run A=tour/m_3_0 BLK=y`)
            do_build(&root, &info);

            let elf = root
                .join("target")
                .join(info.target)
                .join("release")
                .join("arceos-sysmap");
            let bin = elf.with_extension("bin");

            // objcopy for non-x86_64 architectures
            if arch != "x86_64" {
                do_objcopy(&elf, &bin, info.objcopy_arch);
            }

            do_run_qemu(arch, &elf, &bin, &disk);
        }
    }
}
