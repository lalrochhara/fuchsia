// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Result};
use assembly_fvm::FilesystemAttributes;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::PathBuf;

/// The set of information that defines a fuchsia product.
#[derive(Default, Deserialize, Serialize)]
pub struct ProductConfig {
    /// The path to a file indicating the version of the product.
    pub version_file: Option<PathBuf>,

    /// The path to a file on the host indicating the OTA backstop.
    pub epoch_file: Option<PathBuf>,

    /// The packages whose files get added to the base package. The
    /// packages themselves are not added, but their individual files are
    /// extracted and added to the base package. These files are needed
    /// to bootstrap pkgfs.
    #[serde(default)]
    pub extra_packages_for_base_package: Vec<PathBuf>,

    /// The packages that are in the base package list, which is added
    /// to the base package (data/static_packages). These packages get
    /// updated by flashing and OTAing, and cannot be garbage collected.
    #[serde(default)]
    pub base_packages: Vec<PathBuf>,

    /// The packages that are in the cache package list, which is added
    /// to the base package (data/cache_packages). These packages get
    /// updated by flashing and OTAing, but can be garbage collected.
    #[serde(default)]
    pub cache_packages: Vec<PathBuf>,

    /// The path to the prebuilt kernel.
    pub kernel_image: PathBuf,

    /// The list of command line arguments to pass to the kernel on startup.
    #[serde(default)]
    pub kernel_cmdline: Vec<String>,

    /// The set of files to be placed in BOOTFS in the ZBI.
    #[serde(default)]
    pub bootfs_files: Vec<FileEntry>,
}

/// The set of information that defines a fuchsia board.
#[derive(Deserialize, Serialize)]
pub struct BoardConfig {
    /// The name of the board.
    pub board_name: String,

    /// The information required to construct and flash the verified boot
    /// metadata (VBMeta).
    pub vbmeta: Option<VBMetaConfig>,

    /// The list of information for each bootloader.
    #[serde(default)]
    pub bootloaders: Vec<BootloaderEntry>,

    /// The information required to construct and flash the ZBI.
    pub zbi: ZbiConfig,

    /// The information required to construct the BlobFS.
    #[serde(default)]
    pub blobfs: BlobFSConfig,

    /// The information required to construct and flash the FVM.
    pub fvm: Option<FvmConfig>,

    /// The information required to update and flash recovery.
    /// TODO(fxbug.dev/76371): Re-design so that recovery is a separate product.
    pub recovery: Option<RecoveryConfig>,
}

/// A mapping between a file source and destination.
#[derive(Deserialize, Serialize)]
pub struct FileEntry {
    /// The path of the source file.
    pub source: PathBuf,

    /// The destination path to put the file.
    pub destination: String,
}

/// The information required to sign a VBMeta image.
#[derive(Deserialize, Serialize)]
pub struct VBMetaConfig {
    /// The partition name to flash the VBMeta.
    pub partition: String,

    /// The partition name of the kernel included as a VBMeta descriptor.
    pub kernel_partition: String,

    /// The path on the host to the VBMeta key.
    pub key: PathBuf,

    /// The metadata used to verify the VBMeta key.
    pub key_metadata: PathBuf,

    /// Paths to descriptors to add to the VBMeta.
    #[serde(default)]
    pub additional_descriptor_files: Vec<PathBuf>,

    /// An optional salt to use when generating the VBMeta.
    /// This should only be used to verify the VBMeta generated by the image
    /// assembler matches the one generated by the current build system.
    ///
    /// TODO(fxbug.dev/77114): Remove this once we have deleted the GN-based
    /// VBMeta generation.
    pub salt: Option<PathBuf>,
}

/// A bootloader to add to the update package and flash-files.
#[derive(Deserialize, Serialize)]
pub struct BootloaderEntry {
    /// The partition name to flash the bootloader.
    pub partition: String,

    /// The name of the bootloader entry to add to the update manifest.
    pub name: String,

    /// The type of the bootloader.
    #[serde(rename = "type")]
    pub bootloader_type: String,

    /// The path on the host to the bootloader.
    pub source: PathBuf,
}

/// The information required to construct a ZBI.
#[derive(Default, Deserialize, Serialize)]
pub struct ZbiConfig {
    /// The partition name to flash the ZBI.
    pub partition: String,

    /// The name of the ZBI (fuchsia or recovery).
    #[serde(default = "default_zbi_name")]
    pub name: String,

    /// The maximum size of the ZBI in bytes.
    #[serde(default)]
    pub max_size: u64,

    /// Whether the FVM should be added to the ZBI as a RAMDISK.
    #[serde(default = "default_false")]
    pub embed_fvm_in_zbi: bool,

    /// The compression format for the ZBI.
    #[serde(default = "default_zbi_compression")]
    pub compression: String,

    /// An optional "signing script" to sign/repackage the zbi correctly for
    /// use with the device bootloader.
    pub signing_script: Option<ZbiSigningScript>,
}

/// The information needed to custom-package a ZBI for use on a board with
/// a non-standard (for Fuchsia) bootloader
///
/// The tool specified here _must_ take the following arguments:
///  -z <path to ZBI>
///  -o <output path to write to>
///  -B <build dir, relative to tool's pwd>
#[derive(Deserialize, Serialize)]
pub struct ZbiSigningScript {
    /// The path to the tool to use
    pub tool: PathBuf,

    /// Extra arguments to pass to the tool.  These are passed to the tool after
    /// the above-documented, required, arguments, are passed to the tool.
    #[serde(default)]
    pub extra_arguments: Vec<String>,
}

fn default_false() -> bool {
    false
}

fn default_zbi_name() -> String {
    "fuchsia".to_string()
}

fn default_zbi_compression() -> String {
    "zstd".to_string()
}

/// The information required to construct a BlobFS.
#[derive(Default, Deserialize, Serialize)]
pub struct BlobFSConfig {
    /// The layout format of the blobs.
    /// Typically "padded" or "compact"
    #[serde(default = "default_blob_layout")]
    pub layout: String,

    /// Whether to include the update package in blobfs in order to reserve
    /// space. This is usually to ensure that OTAs are possible.
    #[serde(default = "default_true")]
    pub include_update_package: bool,
}

fn default_blob_layout() -> String {
    "compact".to_string()
}

fn default_true() -> bool {
    true
}

/// The information required to construct a FVM.
#[derive(Deserialize, Serialize)]
pub struct FvmConfig {
    /// The partition name to flash the FVM.
    pub partition: String,

    /// The size in bytes of each slice.
    #[serde(default = "default_fvm_slice_size")]
    pub slice_size: u64,

    /// The number of slices reserved in the FVM for internal usage.
    #[serde(default = "default_fvm_reserved_slices")]
    pub reserved_slices: u64,

    /// The list of filesystems to add to the FVM.
    #[serde(default)]
    pub filesystems: Vec<FvmFilesystemEntry>,
}

fn default_fvm_slice_size() -> u64 {
    8388608
}

fn default_fvm_reserved_slices() -> u64 {
    1
}

/// A filesystem to add to the FVM.
#[derive(Deserialize, Serialize)]
pub enum FvmFilesystemEntry {
    /// A BlobFS filesystem.
    BlobFS {
        /// The filesystem attributes of blobfs.
        #[serde(flatten)]
        attributes: FilesystemAttributes,
    },

    /// A MinFS filesystem.
    MinFS {
        /// The path to a prebuilt minfs to add to the FVM.
        path: PathBuf,

        /// The filesystem attributes of minfs.
        #[serde(flatten)]
        attributes: FilesystemAttributes,
    },
}

/// The information required to update and flash recovery.
#[derive(Deserialize, Serialize)]
pub struct RecoveryConfig {
    /// The name of the recovery image in the update package
    pub name: String,

    /// The path on the host to the prebuilt recovery ZBI.
    pub zbi: PathBuf,

    /// The path on the host to the prebuilt recovery VBMeta.
    pub vbmeta: Option<PathBuf>,
}

pub fn from_reader<R, T>(reader: &mut R) -> Result<T>
where
    R: Read,
    T: serde::de::DeserializeOwned,
{
    let mut data = String::default();
    reader.read_to_string(&mut data).context("Cannot read the config")?;
    serde_json::from_str(&data).context("Cannot parse the config")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_from_json_file() {
        let json = r#"
            {
              "version_file": "path/to/version",
              "epoch_file": "path/to/epoch",
              "extra_packages_for_base_package": ["package0"],
              "base_packages": ["package1", "package2"],
              "cache_packages": ["package3", "package4"],
              "kernel_image": "path/to/kernel",
              "kernel_cmdline": ["arg1", "arg2"],
              "bootfs_files": [
                {
                    "source": "path/to/source",
                    "destination": "path/to/destination"
                }
              ]
            }
        "#;

        let mut cursor = std::io::Cursor::new(json);
        let config: ProductConfig = from_reader(&mut cursor).expect("parse config");
        assert_eq!(config.version_file, Some(PathBuf::from("path/to/version")));
    }

    #[test]
    fn product_from_minimal_json_file() {
        let json = r#"
            {
              "version_file": "path/to/version",
              "epoch_file": "path/to/epoch",
              "kernel_image": "path/to/kernel"
            }
        "#;

        let mut cursor = std::io::Cursor::new(json);
        let config: ProductConfig = from_reader(&mut cursor).expect("parse config");
        assert_eq!(config.version_file, Some(PathBuf::from("path/to/version")));
    }

    #[test]
    fn board_from_json_file() {
        let json = r#"
            {
              "board_name": "my-board",
              "vbmeta": {
                "partition": "name",
                "kernel_partition": "zircon",
                "key": "path/to/key",
                "key_metadata": "path/to/metadata"
              },
              "bootloaders": [
                {
                  "partition": "name",
                  "name": "name",
                  "type": "bl2",
                  "source": "path/to/file/on/host"
                }
              ],
              "zbi": {
                "partition": "name",
                "name": "fuchsia",
                "max_size": 100,
                "embed_fvm_in_zbi": false,
                "compression": "zstd.max"
              },
              "fvm": {
                "partition": "name",
                "slice_size": 100,
                "reserved_slices": 100,
                "filesystems": [
                  {
                    "MinFS": {
                      "path": "path/to/data.blk",
                      "name": "data",
                      "minimum_inodes": 100,
                      "minimum_data_size": 100,
                      "maximum_bytes": 100
                    }
                  },
                  {
                    "BlobFS": {
                      "name": "blob",
                      "minimum_inodes": 100,
                      "minimum_data_size": 100,
                      "maximum_bytes": 100
                    }
                  }
                ]
              },
              "recovery": {
                "name": "recovery",
                "zbi": "path/to/recovery.zbi",
                "vbmeta": "path/to/recovery.vbmeta"
              }
            }
         "#;

        let mut cursor = std::io::Cursor::new(json);
        let config: BoardConfig = from_reader(&mut cursor).expect("parse config");
        assert_eq!(config.board_name, "my-board");
    }

    #[test]
    fn board_from_minimal_json_file() {
        let json = r#"
            {
              "board_name": "my-board",
              "vbmeta": {
                "partition": "name",
                "kernel_partition": "zircon",
                "key": "path/to/key",
                "key_metadata": "path/to/metadata"
              },
              "zbi": {
                "partition": "name"
              },
              "recovery": {
                "name": "recovery",
                "zbi": "path/to/recovery.zbi",
                "vbmeta": "path/to/recovery.vbmeta"
              }
            }
         "#;

        let mut cursor = std::io::Cursor::new(json);
        let config: BoardConfig = from_reader(&mut cursor).expect("parse config");
        assert_eq!(config.board_name, "my-board");
    }

    #[test]
    fn product_from_invalid_json_file() {
        let json = r#"
            {
            }
        "#;

        let mut cursor = std::io::Cursor::new(json);
        let config: Result<ProductConfig> = from_reader(&mut cursor);
        assert!(config.is_err());
    }

    #[test]
    fn board_from_invalid_json_file() {
        let json = r#"
            {
            }
        "#;

        let mut cursor = std::io::Cursor::new(json);
        let config: Result<BoardConfig> = from_reader(&mut cursor);
        assert!(config.is_err());
    }
}
