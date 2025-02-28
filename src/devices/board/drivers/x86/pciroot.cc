// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
//
#include <endian.h>
#include <fuchsia/hardware/pciroot/c/banjo.h>
#include <fuchsia/hardware/sysmem/c/banjo.h>
#include <inttypes.h>
#include <lib/ddk/debug.h>
#include <lib/pci/pio.h>
#include <zircon/compiler.h>
#include <zircon/hw/i2c.h>
#include <zircon/syscalls/resource.h>
#include <zircon/types.h>

#include <array>
#include <memory>

#include <acpica/acpi.h>

#include "acpi-private.h"
#include "acpi/device.h"
#include "dev.h"
#include "errors.h"
#include "pci.h"
#include "pci_allocators.h"
#include "src/devices/lib/iommu/iommu.h"

static zx_status_t pciroot_op_get_bti(void* /*context*/, uint32_t bdf, uint32_t index,
                                      zx_handle_t* bti) {
  // The x86 IOMMU world uses PCI BDFs as the hardware identifiers, so there
  // will only be one BTI per device.
  if (index != 0) {
    return ZX_ERR_OUT_OF_RANGE;
  }
  // For dummy IOMMUs, the bti_id just needs to be unique.  For Intel IOMMUs,
  // the bti_ids correspond to PCI BDFs.
  zx_handle_t iommu_handle;
  zx_status_t status = iommu_manager_iommu_for_bdf(bdf, &iommu_handle);
  if (status != ZX_OK) {
    return status;
  }
  return zx_bti_create(iommu_handle, 0, bdf, bti);
}

#ifdef ENABLE_USER_PCI
zx_status_t x64Pciroot::PcirootGetBti(uint32_t bdf, uint32_t index, zx::bti* bti) {
  return pciroot_op_get_bti(nullptr, bdf, index, bti->reset_and_get_address());
}

zx_status_t x64Pciroot::PcirootGetPciPlatformInfo(pci_platform_info_t* info) {
  *info = context_.info;
  info->irq_routing_list = context_.routing.data();
  info->irq_routing_count = context_.routing.size();

  return ZX_OK;
}

zx_status_t x64Pciroot::PcirootConfigRead8(const pci_bdf_t* address, uint16_t offset,
                                           uint8_t* value) {
  return pci_pio_read8(*address, static_cast<uint8_t>(offset), value);
}

zx_status_t x64Pciroot::PcirootConfigRead16(const pci_bdf_t* address, uint16_t offset,
                                            uint16_t* value) {
  return pci_pio_read16(*address, static_cast<uint8_t>(offset), value);
}

zx_status_t x64Pciroot::PcirootConfigRead32(const pci_bdf_t* address, uint16_t offset,
                                            uint32_t* value) {
  return pci_pio_read32(*address, static_cast<uint8_t>(offset), value);
}

zx_status_t x64Pciroot::PcirootConfigWrite8(const pci_bdf_t* address, uint16_t offset,
                                            uint8_t value) {
  return pci_pio_write8(*address, static_cast<uint8_t>(offset), value);
}

zx_status_t x64Pciroot::PcirootConfigWrite16(const pci_bdf_t* address, uint16_t offset,
                                             uint16_t value) {
  return pci_pio_write16(*address, static_cast<uint8_t>(offset), value);
}

zx_status_t x64Pciroot::PcirootConfigWrite32(const pci_bdf_t* address, uint16_t offset,
                                             uint32_t value) {
  return pci_pio_write32(*address, static_cast<uint8_t>(offset), value);
}

zx_status_t x64Pciroot::Create(PciRootHost* root_host, x64Pciroot::Context ctx, zx_device_t* parent,
                               const char* name) {
  auto pciroot = new x64Pciroot(root_host, std::move(ctx), parent, name);
  return pciroot->DdkAdd(name);
}

#else  // TODO(cja): remove after the switch to userspace pci
static zx_status_t pciroot_op_get_pci_platform_info(void*, pci_platform_info_t*) {
  return ZX_ERR_NOT_SUPPORTED;
}

static bool pciroot_op_driver_should_proxy_config(void* /*ctx*/) { return false; }

static zx_status_t pciroot_op_config_read8(void*, const pci_bdf_t*, uint16_t, uint8_t*) {
  return ZX_ERR_NOT_SUPPORTED;
}

static zx_status_t pciroot_op_config_read16(void*, const pci_bdf_t*, uint16_t, uint16_t*) {
  return ZX_ERR_NOT_SUPPORTED;
}

static zx_status_t pciroot_op_config_read32(void*, const pci_bdf_t*, uint16_t, uint32_t*) {
  return ZX_ERR_NOT_SUPPORTED;
}

static zx_status_t pciroot_op_config_write8(void*, const pci_bdf_t*, uint16_t, uint8_t) {
  return ZX_ERR_NOT_SUPPORTED;
}

static zx_status_t pciroot_op_config_write16(void*, const pci_bdf_t*, uint16_t, uint16_t) {
  return ZX_ERR_NOT_SUPPORTED;
}

static zx_status_t pciroot_op_config_write32(void*, const pci_bdf_t*, uint16_t, uint32_t) {
  return ZX_ERR_NOT_SUPPORTED;
}

static zx_status_t pciroot_op_allocate_msi(void*, uint32_t, bool, zx_handle_t*) {
  return ZX_ERR_NOT_SUPPORTED;
}

static zx_status_t pciroot_op_get_address_space(void*, size_t, zx_paddr_t, pci_address_space_t,
                                                bool, zx_paddr_t*, zx_handle_t*, zx_handle_t*) {
  return ZX_ERR_NOT_SUPPORTED;
}

static pciroot_protocol_ops_t pciroot_proto = {
    .get_bti = pciroot_op_get_bti,
    .get_pci_platform_info = pciroot_op_get_pci_platform_info,
    .driver_should_proxy_config = pciroot_op_driver_should_proxy_config,
    .config_read8 = pciroot_op_config_read8,
    .config_read16 = pciroot_op_config_read16,
    .config_read32 = pciroot_op_config_read32,
    .config_write8 = pciroot_op_config_write8,
    .config_write16 = pciroot_op_config_write16,
    .config_write32 = pciroot_op_config_write32,
    .get_address_space = pciroot_op_get_address_space,
    .allocate_msi = pciroot_op_allocate_msi,
};

pciroot_protocol_ops_t* get_pciroot_ops(void) { return &pciroot_proto; }

#endif  // ENABLE_USER_PCI
