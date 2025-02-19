#![deny(missing_docs)]
#![doc = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md"))]

use std::io;
use rayon::prelude::*;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use regex::Regex;

/// Error messages
#[derive(Debug)]
pub enum Error {}

/// Core builder to setup the bindings options
#[derive(Debug)]
pub struct Builder {
    cuda_root: Option<PathBuf>,
    kernel_paths: Vec<PathBuf>,
    include_paths: Vec<PathBuf>,
    // compute_cap: Option<usize>,
    out_dir: PathBuf,
    extra_args: Vec<&'static str>,
}

impl Default for Builder {
    fn default() -> Self {
        // Use only physical cores for rayon.
        // Builds can be super consuming and exhaust resources quite fast
        // like when building flash attention kernels
        let num_cpus = std::env::var("RAYON_NUM_THREADS").map_or_else(
            |_| num_cpus::get_physical(),
            |s| usize::from_str(&s).expect("RAYON_NUM_THREADS is not set to a valid integer"),
        );

        rayon::ThreadPoolBuilder::new()
            .num_threads(num_cpus)
            .build_global()
            .expect("build rayon global threadpool");

        let out_dir = std::env::var("OUT_DIR").expect("Expected OUT_DIR environement variable to be present, is this running within `build.rs`?").into();

        let cuda_root = cuda_include_dir();
        let kernel_paths = default_kernels().unwrap_or_default();
        let include_paths = default_include().unwrap_or_default();
        let extra_args = vec![];
        // let compute_cap = compute_cap().ok();
        Self {
            cuda_root,
            kernel_paths,
            include_paths,
            extra_args,
            // compute_cap,
            out_dir,
        }
    }
}

/// Helper struct to create a rust file when buildings PTX files.
pub struct Bindings {
    write: bool,
    paths: Vec<PathBuf>,
}

fn default_kernels() -> Option<Vec<PathBuf>> {
    Some(
        glob::glob("src/**/*.cu")
            .ok()?
            .map(|p| p.expect("Invalid path"))
            .collect(),
    )
}
fn default_include() -> Option<Vec<PathBuf>> {
    Some(
        glob::glob("src/**/*.cuh")
            .ok()?
            .map(|p| p.expect("Invalid path"))
            .collect(),
    )
}

impl Builder {
    /// Setup the kernel paths. All path must be set at once and be valid files.
    /// ```no_run
    /// let builder = bindgen_cuda::Builder::default().kernel_paths(vec!["src/mykernel.cu"]);
    /// ```
    pub fn kernel_paths<P: Into<PathBuf>>(mut self, paths: Vec<P>) -> Self {
        let paths: Vec<_> = paths.into_iter().map(|p| p.into()).collect();
        let inexistent_paths: Vec<_> = paths.iter().filter(|f| !f.exists()).collect();
        if !inexistent_paths.is_empty() {
            panic!("Kernels paths do not exist {inexistent_paths:?}");
        }
        self.kernel_paths = paths;
        self
    }

    /// Setup the kernel paths. All path must be set at once and be valid files.
    /// ```no_run
    /// let builder = bindgen_cuda::Builder::default().include_paths(vec!["src/mykernel.cuh"]);
    /// ```
    pub fn include_paths<P: Into<PathBuf>>(mut self, paths: Vec<P>) -> Self {
        self.include_paths = paths.into_iter().map(|p| p.into()).collect();
        self
    }

    /// Setup the kernels with a glob.
    /// ```no_run
    /// let builder = bindgen_cuda::Builder::default().kernel_paths_glob("src/**/*.cu");
    /// ```
    pub fn kernel_paths_glob(mut self, glob: &str) -> Self {
        self.kernel_paths = glob::glob(glob)
            .expect("Invalid blob")
            .map(|p| p.expect("Invalid path"))
            .collect();
        self
    }

    /// Setup the include files with a glob.
    /// ```no_run
    /// let builder = bindgen_cuda::Builder::default().kernel_paths_glob("src/**/*.cuh");
    /// ```
    pub fn include_paths_glob(mut self, glob: &str) -> Self {
        self.include_paths = glob::glob(glob)
            .expect("Invalid blob")
            .map(|p| p.expect("Invalid path"))
            .collect();
        self
    }

    /// Modifies the output directory.
    /// By default this is
    /// [OUT_DIR](https://doc.rust-lang.org/cargo/reference/environment-variables.html#environment-variables-cargo-sets-for-build-scripts)
    /// ```no_run
    /// let builder = bindgen_cuda::Builder::default().out_dir("out/");
    /// ```
    pub fn out_dir<P: Into<PathBuf>>(mut self, out_dir: P) -> Self {
        self.out_dir = out_dir.into();
        self
    }

    /// Sets up extra nvcc compile arguments.
    /// ```no_run
    /// let builder = bindgen_cuda::Builder::default().arg("--expt-relaxed-constexpr");
    /// ```
    pub fn arg(mut self, arg: &'static str) -> Self {
        self.extra_args.push(arg);
        self
    }

    /// Forces the cuda root to a specific directory.
    /// By default all standard directories will be visited.
    /// ```no_run
    /// let builder = bindgen_cuda::Builder::default().cuda_root("/usr/local/cuda");
    /// ```
    pub fn cuda_root<P>(&mut self, path: P)
        where
            P: Into<PathBuf>,
    {
        self.cuda_root = Some(path.into());
    }

    /// Consumes the builder and create a lib in the out_dir.
    /// It then needs to be linked against in your `build.rs`
    /// ```no_run
    /// let builder = bindgen_cuda::Builder::default().build_lib("libflash.a");
    /// println!("cargo:rustc-link-lib=flash");
    /// ```
    pub fn build_lib<P>(self, out_file: P)
        where
            P: Into<PathBuf>,
    {
        let out_file = out_file.into();
        // let compute_cap = self.compute_cap.expect("Failed to get compute_cap");
        let out_dir = self.out_dir;
        let cu_files: Vec<_> = self
            .kernel_paths
            .iter()
            .map(|f| {
                let mut obj_file = out_dir.join(
                    f.file_name()
                        .expect("kernels paths should include a filename"),
                );
                obj_file.set_extension("o");
                (f, obj_file)
            })
            .collect();
        let out_modified: Result<_, _> = out_file.metadata().and_then(|m| m.modified());
        let should_compile = if let Ok(out_modified) = out_modified {
            self.kernel_paths.iter().any(|entry| {
                let in_modified = entry
                    .metadata()
                    .expect("kernel {entry} should exist")
                    .modified()
                    .expect("kernel modified to be accessible");
                in_modified.duration_since(out_modified).is_ok()
            })
        } else {
            true
        };
        let ccbin_env = std::env::var("NVCC_CCBIN");
        if should_compile {
            cu_files
                .par_iter()
                .map(|(cu_file, obj_file)| {
                    let mut command = std::process::Command::new("hipcc");
                    command
                        // .arg(format!("--gpu-architecture=sm_{compute_cap}"))
                        .arg("-c")
                        .args(["-o", obj_file.to_str().expect("valid outfile")])
                        .args(["--default-stream", "per-thread"])
                        .args(&self.extra_args);
                    if let Ok(ccbin_path) = &ccbin_env {
                        command
                            .arg("-allow-unsupported-compiler")
                            .args(["-ccbin", ccbin_path]);
                    }
                    command.arg(cu_file);
                    let output = command
                        .spawn()
                        .expect("failed spawning nvcc")
                        .wait_with_output().expect("capture nvcc output");
                    if !output.status.success() {
                        panic!(
                            "nvcc error while executing compiling: {:?}\n\n# stdout\n{:#}\n\n# stderr\n{:#}",
                            &command,
                            String::from_utf8_lossy(&output.stdout),
                            String::from_utf8_lossy(&output.stderr)
                        )
                    }
                    Ok(())
                })
                .collect::<Result<(), std::io::Error>>().expect("compile files correctly");
            let obj_files = cu_files.iter().map(|c| c.1.clone()).collect::<Vec<_>>();
            let mut command = std::process::Command::new("hipcc");
            command
                .arg("--lib")
                .args([
                    "-o",
                    out_file.to_str().expect("library file {out_file} to exist"),
                ])
                .args(obj_files);
            let output = command
                .spawn()
                .expect("failed spawning nvcc")
                .wait_with_output()
                .expect("Run nvcc");
            if !output.status.success() {
                panic!(
                    "nvcc error while linking: {:?}\n\n# stdout\n{:#}\n\n# stderr\n{:#}",
                    &command,
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                )
            }
        }
    }

    /// Consumes the builder and outputs 1 ptx file for each kernels
    /// found.
    /// This function returns [`Bindings`] which can then be unused
    /// to create a rust source file that will include those kernels.
    /// ```no_run
    /// let bindings = bindgen_cuda::Builder::default().build_ptx().unwrap();
    /// bindings.write("src/lib.rs").unwrap();
    /// ```
    pub fn build_ptx(self) -> Result<Bindings, Error> {
        let cuda_root = self.cuda_root.expect("Could not find CUDA in standard locations, set it manually using Builder().set_cuda_root(...)");
        // let compute_cap = self.compute_cap.expect("Could not find compute_cap");
        println!(
            "cargo:rustc-env=CUDA_INCLUDE_DIR={}",
            cuda_root.join("include").display()
        );
        let out_dir = self.out_dir;

        let mut include_paths = self.include_paths;
        for path in &mut include_paths {
            println!("cargo:rerun-if-changed={}", path.display());
            let destination =
                out_dir.join(path.file_name().expect("include path to have filename"));
            std::fs::copy(path.clone(), destination).expect("copy include headers");
            // remove the filename from the path so it's just the directory
            path.pop();
        }

        include_paths.sort();
        include_paths.dedup();

        #[allow(unused)]
            let include_options: Vec<String> = include_paths
            .into_iter()
            .map(|s| {
                "-I".to_string()
                    + &s.into_os_string()
                    .into_string()
                    .expect("include option to be valid string")
            })
            .collect::<Vec<_>>();

        let ccbin_env = std::env::var("NVCC_CCBIN");
        println!("cargo:rerun-if-env-changed=NVCC_CCBIN");
        let children = self.kernel_paths
            .par_iter()
            .flat_map(|p| {
                println!("cargo:rerun-if-changed={}", p.display());
                let mut output = p.clone();
                output.set_extension("o");
                let output_filename = std::path::Path::new(&out_dir).to_path_buf().join("out").with_file_name(output.file_name().expect("kernel to have a filename"));

                let ignore = if let Ok(metadata) = output_filename.metadata() {
                    let out_modified = metadata.modified().expect("modified to be accessible");
                    let in_modified = p.metadata().expect("input to have metadata").modified().expect("input metadata to be accessible");
                    out_modified.duration_since(in_modified).is_ok()
                } else {
                    false
                };
                if ignore {
                    None
                } else {
                    let mut command = std::process::Command::new("hipcc");
                    // We get a list of all existing amd gpu arch newer than gfx900 (ie: arch compatible with rocm)
                    let amd_arches = get_amdgpu_arch("gfx900");
                    let offload_arch:Vec<_> =  amd_arches.into_iter().map(|arch| format!("--offload-arch={arch}")).collect();
                    command //.arg(format!("--gpu-architecture=sm_{compute_cap}"))
                        // .arg("--ptx")
                        // .args(["--default-stream", "per-thread"])
                        // .args(["--output-directory", &out_dir.display().to_string()])
                        .args(["-O3","-std=c++17","-ffast-math","--cuda-device-only","--genco", "-include", "hip/hip_runtime.h", "-parallel-jobs=15"])
                        .args(&offload_arch)
                        .args(["-o", &output_filename.display().to_string()])
                        .args(&self.extra_args)
                        .args(&include_options);
                    if let Ok(ccbin_path) = &ccbin_env {
                        command
                            .arg("-allow-unsupported-compiler")
                            .args(["-ccbin", ccbin_path]);
                    }
                    command.arg(p);
                    println!("{:?}", command);
                    Some((p, command.spawn()
                        .expect("nvcc failed to start. Ensure that you have CUDA installed and that `nvcc` is in your PATH.").wait_with_output()))
                }
            })
            .collect::<Vec<_>>();

        let ptx_paths: Vec<PathBuf> = glob::glob(&format!("{0}/**/*.ptx", out_dir.display()))
            .expect("valid glob")
            .map(|p| p.expect("valid path for PTX"))
            .collect();
        // We should rewrite `src/lib.rs` only if there are some newly compiled kernels, or removed
        // some old ones
        let write = !children.is_empty() || self.kernel_paths.len() < ptx_paths.len();
        for (kernel_path, child) in children {
            let output = child.expect("nvcc failed to run. Ensure that you have CUDA installed and that `nvcc` is in your PATH.");
            assert!(
                output.status.success(),
                "nvcc error while compiling {kernel_path:?}:\n\n# stdout\n{:#}\n\n# stderr\n{:#}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(Bindings {
            write,
            paths: self.kernel_paths,
        })
    }
}

impl Bindings {
    /// Writes a helper rust file that will include the PTX sources as
    /// `const KERNEL_NAME` making it easier to interact with the PTX sources.
    pub fn write<P>(&self, out: P) -> Result<(), Error>
        where
            P: AsRef<Path>,
    {
        if self.write {
            let mut file = std::fs::File::create(out).expect("Create lib in {out}");
            for kernel_path in &self.paths {
                let name = kernel_path
                    .file_stem()
                    .expect("kernel to have stem")
                    .to_str()
                    .expect("kernel path to be valid");
                file.write_all(
                    format!(
                        r#"pub const {}: &'static [u8] = include_bytes!(concat!(env!("OUT_DIR"), "/{}.o"));"#,
                        name.to_uppercase().replace('.', "_"),
                        name
                    )
                        .as_bytes(),
                )
                    .expect("write to {out}");
                file.write_all(&[b'\n']).expect("write to {out}");
            }
        }
        Ok(())
    }
}

fn cuda_include_dir() -> Option<PathBuf> {
    // NOTE: copied from cudarc build.rs.
    let env_vars = [
        "CUDA_PATH",
        "CUDA_ROOT",
        "CUDA_TOOLKIT_ROOT_DIR",
        "CUDNN_LIB",
    ];
    #[allow(unused)]
        let env_vars = env_vars
        .into_iter()
        .map(std::env::var)
        .filter_map(Result::ok)
        .map(Into::<PathBuf>::into);

    let roots = [
        "/usr",
        "/usr/local/cuda",
        "/opt/rocm",
        "/usr/lib/cuda",
        "C:/Program Files/NVIDIA GPU Computing Toolkit",
        "C:/CUDA",
    ];

    println!("cargo:info={roots:?}");

    #[allow(unused)]
        let roots = roots.into_iter().map(Into::<PathBuf>::into);

    #[cfg(feature = "ci-check")]
        let root: PathBuf = "ci".into();

    #[cfg(not(feature = "ci-check"))]
    env_vars
        .chain(roots)
        .find(|path| path.join("include").join("hip").join("hip_common.h").is_file())
}

////////////////////////////////////////////////////////////////////////////////////////////////////
// Extract available amd gpu arch using llvm
// See Orochi project file https://github.com/GPUOpen-LibrariesAndSDKs/Orochi/blob/65de35c89c3f4dab8156ff7e5df774b7a8aa9bb6/scripts/kernelCompile.py#L9

/// Convert amd gpu arch to a number
fn amdgpu_arch_to_num(arch: &str) -> usize {
    let min_arch = &arch[3..];
    usize::from_str_radix(min_arch,16).expect("min_arch is not a valid amd arch (eg gfx1030)")
}

/// Extract available amd gpu arch using llc command from llvm
fn get_amdgpu_arch(min_arch: &str) -> Vec<String> {
    let min_arch = amdgpu_arch_to_num(min_arch);
    let mut command = std::process::Command::new("llc");
    command
        .args(["-march=amdgcn","-mcpu=help"]);
    let output = command.output().expect("Cannot start llc, is llvm installed ?");
    let output = String::from_utf8(output.stdout).unwrap();

    let regex = Regex::new(r"(?m)\s+(gfx[0-9a-f]+).*processor.").unwrap();

    regex.captures_iter(&output).map(|c| c.extract()).map(|(_,[arch])| {
        arch.to_string()
    }).filter(|arch| amdgpu_arch_to_num(arch) >= min_arch).collect()
}

////////////////////////////////////////////////////////////////////////////////////////////////////

// fn compute_cap() -> Result<usize, Error> {
//     println!("cargo:rerun-if-env-changed=CUDA_COMPUTE_CAP");
//
//     // Try to parse compute caps from env
//     let compute_cap = if let Ok(compute_cap_str) = std::env::var("CUDA_COMPUTE_CAP") {
//         println!("cargo:rustc-env=CUDA_COMPUTE_CAP={compute_cap_str}");
//         compute_cap_str
//             .parse::<usize>()
//             .expect("Could not parse code")
//     } else {
//         // Use rocm-smi to get the current compute cap
//         let out = std::process::Command::new("rocm-smi")
//             .arg("--query-gpu=compute_cap")
//             .arg("--format=csv")
//             .output()
//             .expect("`rocm-smi` failed. Ensure that you have CUDA installed and that `rocm-smi` is in your PATH.");
//         let out = std::str::from_utf8(&out.stdout).expect("stdout is not a utf8 string");
//         let mut lines = out.lines();
//         assert_eq!(lines.next().expect("missing line in stdout"), "compute_cap");
//         let cap = lines
//             .next()
//             .expect("missing line in stdout")
//             .replace('.', "");
//         let cap = cap.parse::<usize>().expect("cannot parse as int {cap}");
//         println!("cargo:rustc-env=CUDA_COMPUTE_CAP={cap}");
//         cap
//     };
//
//     // Grab available GPU codes from nvcc and select the highest one
//     let (supported_nvcc_codes, max_nvcc_code) = {
//         let out = std::process::Command::new("hipcc")
//             .arg("--list-gpu-code")
//             .output()
//             .expect("`nvcc` failed. Ensure that you have CUDA installed and that `nvcc` is in your PATH.");
//         let out = std::str::from_utf8(&out.stdout).expect("valid utf-8 nvcc output");
//
//         let out = out.lines().collect::<Vec<&str>>();
//         let mut codes = Vec::with_capacity(out.len());
//         for code in out {
//             let code = code.split('_').collect::<Vec<&str>>();
//             if !code.is_empty() && code.contains(&"sm") {
//                 if let Ok(num) = code[1].parse::<usize>() {
//                     codes.push(num);
//                 }
//             }
//         }
//         codes.sort();
//         let max_nvcc_code = *codes.last().expect("no gpu codes parsed from nvcc");
//         (codes, max_nvcc_code)
//     };
//
//     // Check that nvcc supports the asked compute caps
//     if !supported_nvcc_codes.contains(&compute_cap) {
//         panic!(
//             "nvcc cannot target gpu arch {compute_cap}. Available nvcc targets are {supported_nvcc_codes:?}."
//         );
//     }
//     if compute_cap > max_nvcc_code {
//         panic!(
//             "CUDA compute cap {compute_cap} is higher than the highest gpu code from nvcc {max_nvcc_code}"
//         );
//     }
//
//     Ok(compute_cap)
// }

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let builder = Builder::default();
    println!("cargo:info={builder:?}");
    let bindings = builder.build_ptx().unwrap();
    bindings.write("src/lib.rs").unwrap();
}
