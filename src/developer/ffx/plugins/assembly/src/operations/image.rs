// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::config::{
    from_reader, BoardConfig, FvmConfig, FvmFilesystemEntry, ProductConfig, VBMetaConfig,
    ZbiSigningScript,
};
use crate::vfs::RealFilesystemProvider;
use anyhow::{anyhow, Context, Result};
use assembly_base_package::BasePackageBuilder;
use assembly_blobfs::BlobFSBuilder;
use assembly_fvm::{Filesystem, FvmBuilder};
use assembly_update_package::UpdatePackageBuilder;
use assembly_util::PathToStringExt;
use ffx_assembly_args::ImageArgs;
use fuchsia_hash::Hash;
use fuchsia_merkle::MerkleTree;
use fuchsia_pkg::{PackageManifest, PackagePath};
use log::info;
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use vbmeta::Salt;
use zbi::ZbiBuilder;

pub fn assemble(args: ImageArgs) -> Result<()> {
    let ImageArgs { product, board, outdir, gendir, full: _ } = args;

    info!("Loading configuration files.");
    info!("  product:  {}", product.display());
    info!("    board:  {}", board.display());

    let (product, board) = read_configs(product, board)?;
    let gendir = gendir.unwrap_or(outdir.clone());

    let base_package: Option<BasePackage> = if has_base_package(&product) {
        info!("Creating base package");
        Some(construct_base_package(&outdir, &gendir, &product)?)
    } else {
        info!("Skipping base package creation");
        None
    };

    info!("Creating the ZBI");
    let zbi_path = construct_zbi(&outdir, &gendir, &product, &board, base_package.as_ref())?;

    let vbmeta_path: Option<PathBuf> = if let Some(vbmeta_config) = &board.vbmeta {
        info!("Creating the VBMeta image");
        Some(construct_vbmeta(&outdir, vbmeta_config, &zbi_path)?)
    } else {
        info!("Skipping vbmeta creation");
        None
    };

    // If the board specifies a vendor-specific signing script, use that to
    // post-process the ZBI, and then use the post-processed ZBI in the update
    // package and the
    let zbi_for_update_path = if let Some(signing_config) = &board.zbi.signing_script {
        info!("Vendor signing the ZBI");
        vendor_sign_zbi(&outdir, signing_config, &zbi_path)?
    } else {
        zbi_path
    };

    info!("Creating the update package");
    let update_package: UpdatePackage = construct_update(
        &outdir,
        &gendir,
        &product,
        &board,
        &zbi_for_update_path,
        vbmeta_path,
        base_package.as_ref(),
    )?;

    let blobfs_path: Option<PathBuf> = if let Some(base_package) = &base_package {
        info!("Creating the blobfs");
        // Include the update package in blobfs if necessary.
        let update_package =
            if board.blobfs.include_update_package { Some(&update_package) } else { None };
        Some(construct_blobfs(&outdir, &gendir, &product, &board, &base_package, update_package)?)
    } else {
        info!("Skipping blobfs creation");
        None
    };

    let _fvm_path: Option<PathBuf> = if let Some(fvm_config) = &board.fvm {
        info!("Creating the fvm");
        Some(construct_fvm(&outdir, &fvm_config, blobfs_path.as_ref())?)
    } else {
        info!("Skipping fvm creation");
        None
    };

    Ok(())
}

fn read_configs(
    product: impl AsRef<Path>,
    board: impl AsRef<Path>,
) -> Result<(ProductConfig, BoardConfig)> {
    let mut product = File::open(product)?;
    let mut board = File::open(board)?;
    let product: ProductConfig =
        from_reader(&mut product).context("Failed to read the product config")?;
    let board: BoardConfig = from_reader(&mut board).context("Failed to read the board config")?;
    Ok((product, board))
}

fn has_base_package(product: &ProductConfig) -> bool {
    return !(product.base_packages.is_empty()
        && product.cache_packages.is_empty()
        && product.extra_packages_for_base_package.is_empty());
}

struct BasePackage {
    merkle: Hash,
    contents: BTreeMap<String, String>,
    path: PathBuf,
}

fn construct_base_package(
    outdir: impl AsRef<Path>,
    gendir: impl AsRef<Path>,
    product: &ProductConfig,
) -> Result<BasePackage> {
    let mut base_pkg_builder = BasePackageBuilder::default();
    for pkg_manifest_path in &product.extra_packages_for_base_package {
        let pkg_manifest = pkg_manifest_from_path(pkg_manifest_path)?;
        base_pkg_builder.add_files_from_package(pkg_manifest);
    }
    for pkg_manifest_path in &product.base_packages {
        let pkg_manifest = pkg_manifest_from_path(pkg_manifest_path)?;
        base_pkg_builder.add_base_package(pkg_manifest).context(format!(
            "Failed to add package to base package list with manifest: {}",
            pkg_manifest_path.display()
        ))?;
    }
    for pkg_manifest_path in &product.cache_packages {
        let pkg_manifest = pkg_manifest_from_path(pkg_manifest_path)?;
        base_pkg_builder.add_cache_package(pkg_manifest).context(format!(
            "Failed to add package to cache package list with manifest: {}",
            pkg_manifest_path.display()
        ))?;
    }

    let base_package_path = outdir.as_ref().join("base.far");
    let mut base_package = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&base_package_path)
        .context("Failed to create the base package file")?;
    base_package.set_len(0).context("Failed to erase the old base package")?;
    let build_results = base_pkg_builder
        .build(gendir, &mut base_package)
        .context("Failed to build the base package")?;

    base_package.seek(SeekFrom::Start(0))?;
    let base_merkle = MerkleTree::from_reader(&base_package)
        .context("Failed to calculate the base merkle")?
        .root();
    info!("Base merkle: {}", &base_merkle);

    Ok(BasePackage {
        merkle: base_merkle,
        contents: build_results.contents,
        path: base_package_path,
    })
}

fn pkg_manifest_from_path(path: impl AsRef<Path>) -> Result<PackageManifest> {
    let manifest_file = File::open(path)?;
    let pkg_manifest_reader = BufReader::new(manifest_file);
    serde_json::from_reader(pkg_manifest_reader).map_err(Into::into)
}

fn construct_zbi(
    outdir: impl AsRef<Path>,
    gendir: impl AsRef<Path>,
    product: &ProductConfig,
    board: &BoardConfig,
    base_package: Option<&BasePackage>,
) -> Result<PathBuf> {
    let mut zbi_builder = ZbiBuilder::default();

    // Add the kernel image.
    zbi_builder.set_kernel(&product.kernel_image);

    // Instruct devmgr that a /system volume is required.
    zbi_builder.add_boot_arg("devmgr.require-system=true");

    // If a base merkle is supplied, then add the boot arguments for startup up pkgfs with the
    // merkle of the Base Package.
    if let Some(base_package) = &base_package {
        // Specify how to launch pkgfs: bin/pkgsvr <base-merkle>
        zbi_builder
            .add_boot_arg(&format!("zircon.system.pkgfs.cmd=bin/pkgsvr+{}", &base_package.merkle));

        // Add the pkgfs blobs to the boot arguments, so that pkgfs can be bootstrapped out of blobfs,
        // before the blobfs service is available.
        let pkgfs_manifest: PackageManifest = product
            .base_packages
            .iter()
            .find_map(|p| {
                if let Ok(m) = pkg_manifest_from_path(p) {
                    if m.name() == "pkgfs" {
                        return Some(m);
                    }
                }
                return None;
            })
            .context("Failed to find pkgfs in the base packages")?;

        pkgfs_manifest.into_blobs().into_iter().filter(|b| b.path != "meta/").for_each(|b| {
            zbi_builder.add_boot_arg(&format!("zircon.system.pkgfs.file.{}={}", b.path, b.merkle));
        });
    }

    // Add the command line.
    for cmd in &product.kernel_cmdline {
        zbi_builder.add_cmdline_arg(cmd);
    }

    // Add the BootFS files.
    for bootfs_entry in &product.bootfs_files {
        zbi_builder.add_bootfs_file(&bootfs_entry.source, &bootfs_entry.destination);
    }

    // Set the zbi compression to use.
    zbi_builder.set_compression(&board.zbi.compression);

    // Create an output manifest that describes the contents of the built ZBI.
    zbi_builder.set_output_manifest(&gendir.as_ref().join("zbi.json"));

    // Build and return the ZBI.
    let zbi_path = if board.zbi.signing_script.is_none() {
        outdir.as_ref().join("fuchsia.zbi")
    } else {
        outdir.as_ref().join("fuchsia.zbi.unsigned")
    };
    zbi_builder.build(gendir, zbi_path.as_path())?;
    Ok(zbi_path)
}

/// If the board requires the zbi to be post-processed to make it bootable by
/// the bootloaders, then perform that task here.
fn vendor_sign_zbi(
    outdir: impl AsRef<Path>,
    signing_config: &ZbiSigningScript,
    zbi: impl AsRef<Path>,
) -> Result<PathBuf> {
    // The resultant file path
    let signed_path = outdir.as_ref().join("fuchsia.zbi");

    // The parameters of the script that are required:
    let mut args = Vec::new();
    args.push("-z".to_string());
    args.push(zbi.as_ref().path_to_string()?);
    args.push("-o".to_string());
    args.push(signed_path.path_to_string()?);

    // If the script config defines extra arguments, add them:
    args.extend_from_slice(&signing_config.extra_arguments[..]);

    let output = Command::new(&signing_config.tool)
        .args(&args)
        .output()
        .context(format!("Failed to run the vendor tool: {}", signing_config.tool.display()))?;

    // The tool ran, but may have returned an error.
    if output.status.success() {
        Ok(signed_path)
    } else {
        Err(anyhow!(
            "Vendor tool returned an error: {}\n{}",
            output.status,
            String::from_utf8_lossy(output.stderr.as_slice())
        ))
    }
}

fn construct_vbmeta(
    outdir: impl AsRef<Path>,
    vbmeta: &VBMetaConfig,
    zbi: impl AsRef<Path>,
) -> Result<PathBuf> {
    // Generate the salt, or use one provided by the board.
    let salt = match &vbmeta.salt {
        Some(salt_path) => {
            let salt_str = std::fs::read_to_string(salt_path)?;
            Salt::decode_hex(&salt_str)?
        }
        _ => Salt::random()?,
    };

    // Sign the image and construct a VBMeta.
    let (vbmeta, _salt) = crate::vbmeta::sign(
        &vbmeta.kernel_partition,
        zbi,
        &vbmeta.key,
        &vbmeta.key_metadata,
        vbmeta.additional_descriptor_files.clone(),
        salt,
        &RealFilesystemProvider {},
    )?;

    // Write VBMeta to a file and return the path.
    let vbmeta_path = outdir.as_ref().join("fuchsia.vbmeta");
    std::fs::write(&vbmeta_path, vbmeta.as_bytes())?;
    Ok(vbmeta_path)
}

struct UpdatePackage {
    contents: BTreeMap<String, String>,
    path: PathBuf,
}

fn construct_update(
    outdir: impl AsRef<Path>,
    gendir: impl AsRef<Path>,
    product: &ProductConfig,
    board: &BoardConfig,
    zbi: impl AsRef<Path>,
    vbmeta: Option<impl AsRef<Path>>,
    base_package: Option<&BasePackage>,
) -> Result<UpdatePackage> {
    // Create the board name file.
    // TODO(fxbug.dev/76326): Create a better system for writing intermediate files.
    let board_name = gendir.as_ref().join("board");
    std::fs::write(&board_name, &board.board_name)?;

    // Add several files to the update package.
    let mut update_pkg_builder = UpdatePackageBuilder::new();
    if let Some(epoch_file) = &product.epoch_file {
        update_pkg_builder.add_file(epoch_file, "epoch.json")?;
    }
    if let Some(version_file) = &product.version_file {
        update_pkg_builder.add_file(version_file, "version")?;
    }
    update_pkg_builder.add_file(&board_name, "board")?;
    update_pkg_builder.add_file(zbi, &board.zbi.name)?;

    if let Some(vbmeta) = vbmeta {
        update_pkg_builder.add_file(vbmeta, "fuchsia.vbmeta")?;
    }

    if let Some(recovery_config) = &board.recovery {
        update_pkg_builder.add_file(&recovery_config.zbi, &recovery_config.name)?;

        // TODO(fxbug.dev/77997)
        // TODO(fxbug.dev/77535)
        // TODO(fxbug.dev/76371)
        //
        // Determine what to do with this case:
        //  - is it an error if fuchsia.vbmeta is present, but recovery.vbmeta isn't?
        //  - do we generate/sign our own recovery.vbmeta?
        //  - etc.
        if let Some(recovery_vbmeta) = &recovery_config.vbmeta {
            update_pkg_builder.add_file(recovery_vbmeta, "recovery.vbmeta")?;
        }
    }

    // Add the bootloaders.
    for bootloader in &board.bootloaders {
        update_pkg_builder.add_file(&bootloader.source, &bootloader.name)?;
    }

    // Add the packages that need to be updated.
    let mut add_packages_to_update = |packages: &Vec<PathBuf>| -> Result<()> {
        for package_path in packages {
            let manifest = pkg_manifest_from_path(package_path)?;
            update_pkg_builder.add_package_by_manifest(manifest)?;
        }
        Ok(())
    };
    add_packages_to_update(&product.base_packages)?;
    add_packages_to_update(&product.cache_packages)?;

    if let Some(base_package) = &base_package {
        // Add the base package merkle.
        // TODO(fxbug.dev/76986): Do not hardcode the base package path.
        update_pkg_builder.add_package(
            PackagePath::from_name_and_variant("system_image", "0")?,
            base_package.merkle,
        )?;
    }

    // Build the update package and return its path.
    let update_package_path = outdir.as_ref().join("update.far");
    let mut update_package = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&update_package_path)
        .context("Failed to create the update package file")?;
    update_package.set_len(0).context("Failed to erase the old update package")?;
    let update_contents = update_pkg_builder
        .build(gendir, &mut update_package)
        .context("Failed to build the update package")?;
    Ok(UpdatePackage { contents: update_contents, path: update_package_path })
}

fn construct_blobfs(
    outdir: impl AsRef<Path>,
    gendir: impl AsRef<Path>,
    product: &ProductConfig,
    board: &BoardConfig,
    base_package: &BasePackage,
    update_package: Option<&UpdatePackage>,
) -> Result<PathBuf> {
    let mut blobfs_builder = BlobFSBuilder::new(&board.blobfs.layout);

    // Add the base and cache packages.
    for package_manifest_path in &product.base_packages {
        blobfs_builder.add_package(&package_manifest_path)?;
    }
    for package_manifest_path in &product.cache_packages {
        blobfs_builder.add_package(&package_manifest_path)?;
    }

    // Add the base package and its contents.
    blobfs_builder.add_file(&base_package.path)?;
    for (_, source) in &base_package.contents {
        blobfs_builder.add_file(source)?;
    }

    // Add the update package and its contents.
    if let Some(update_package) = update_package {
        blobfs_builder.add_file(&update_package.path)?;
        for (_, source) in &update_package.contents {
            blobfs_builder.add_file(source)?;
        }
    }

    // Build the blobfs and return its path.
    let blobfs_path = outdir.as_ref().join("blob.blk");
    blobfs_builder.build(gendir, &blobfs_path).context("Failed to build the blobfs")?;
    Ok(blobfs_path)
}

fn construct_fvm(
    outdir: impl AsRef<Path>,
    fvm_config: &FvmConfig,
    blobfs_path: Option<impl AsRef<Path>>,
) -> Result<PathBuf> {
    // Create the builder.
    let fvm_path = outdir.as_ref().join("fvm.blk");
    let mut fvm_builder =
        FvmBuilder::new(&fvm_path, fvm_config.slice_size, fvm_config.reserved_slices);

    // Add all the filesystems.
    for entry in &fvm_config.filesystems {
        let (path, attributes) = match entry {
            FvmFilesystemEntry::BlobFS { attributes } => {
                let path = match &blobfs_path {
                    Some(path) => path.as_ref(),
                    None => {
                        anyhow::bail!("BlobFS configuration exists, but BlobFS was not generated");
                    }
                };
                (path.to_path_buf(), attributes.clone())
            }
            FvmFilesystemEntry::MinFS { path, attributes } => {
                (path.to_path_buf(), attributes.clone())
            }
        };
        fvm_builder.filesystem(Filesystem { path, attributes });
    }

    // Construct the FVM.
    fvm_builder.build()?;
    Ok(fvm_path)
}
