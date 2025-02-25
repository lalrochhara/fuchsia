// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "usb-xhci.h"

#include <lib/ddk/debug.h>
#include <lib/ddk/hw/arch_ops.h>
#include <lib/ddk/hw/reg.h>
#include <lib/ddk/platform-defs.h>
#include <lib/device-protocol/pci.h>
#include <lib/zx/bti.h>
#include <lib/zx/interrupt.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <threads.h>
#include <zircon/errors.h>
#include <zircon/syscalls.h>
#include <zircon/types.h>

#include <algorithm>
#include <memory>

#include <fbl/alloc_checker.h>

#include "src/devices/usb/drivers/xhci/usb_xhci_bind.h"
#include "xhci-device-manager.h"
#include "xhci-root-hub.h"
#include "xhci-util.h"
#include "xhci.h"

namespace usb_xhci {

#define MAX_SLOTS 255

#define PDEV_MMIO_INDEX 0
#define PDEV_IRQ_INDEX 0

zx_status_t xhci_add_device(xhci_t* xhci, int slot_id, int hub_address, int speed) {
  zxlogf(DEBUG, "xhci_add_new_device");

  if (!xhci->bus.ops) {
    zxlogf(ERROR, "no bus device in xhci_add_device");
    return ZX_ERR_INTERNAL;
  }

  return usb_bus_interface_add_device(&xhci->bus, slot_id, hub_address, speed);
}

void xhci_remove_device(xhci_t* xhci, int slot_id) {
  zxlogf(DEBUG, "xhci_remove_device %d", slot_id);

  if (!xhci->bus.ops) {
    zxlogf(ERROR, "no bus device in xhci_remove_device");
    return;
  }

  usb_bus_interface_remove_device(&xhci->bus, slot_id);
}

void UsbXhci::UsbHciRequestQueue(usb_request_t* usb_request,
                                 const usb_request_complete_callback_t* complete_cb) {
  xhci_request_queue(xhci_.get(), usb_request, complete_cb);
}

void UsbXhci::UsbHciSetBusInterface(const usb_bus_interface_protocol_t* bus_intf) {
  if (bus_intf) {
    memcpy(&xhci_->bus, bus_intf, sizeof(xhci_->bus));
    // wait until bus driver has started before doing this
    xhci_queue_start_root_hubs(xhci_.get());
  } else {
    memset(&xhci_->bus, 0, sizeof(xhci_->bus));
  }
}

size_t UsbXhci::UsbHciGetMaxDeviceCount() { return xhci_->max_slots + XHCI_RH_COUNT + 1; }

zx_status_t UsbXhci::UsbHciEnableEndpoint(uint32_t device_id,
                                          const usb_endpoint_descriptor_t* ep_desc,
                                          const usb_ss_ep_comp_descriptor_t* ss_com_desc,
                                          bool enable) {
  if (enable) {
    return xhci_enable_endpoint(xhci_.get(), device_id, ep_desc, ss_com_desc);
  } else {
    return xhci_disable_endpoint(xhci_.get(), device_id, ep_desc);
  }
}

uint64_t UsbXhci::UsbHciGetCurrentFrame() { return xhci_get_current_frame(xhci_.get()); }

zx_status_t UsbXhci::UsbHciConfigureHub(uint32_t device_id, usb_speed_t speed,
                                        const usb_hub_descriptor_t* desc, bool multi_tt) {
  return xhci_configure_hub(xhci_.get(), device_id, speed, desc);
}

zx_status_t UsbXhci::UsbHciHubDeviceAdded(uint32_t device_id, uint32_t port, usb_speed_t speed) {
  return xhci_enumerate_device(xhci_.get(), device_id, port, speed);
}

zx_status_t UsbXhci::UsbHciHubDeviceRemoved(uint32_t device_id, uint32_t port) {
  xhci_device_disconnected(xhci_.get(), device_id, port);
  return ZX_OK;
}

zx_status_t UsbXhci::UsbHciHubDeviceReset(uint32_t device_id, uint32_t port) {
  return xhci_device_reset(xhci_.get(), device_id, port);
}

zx_status_t UsbXhci::UsbHciResetEndpoint(uint32_t device_id, uint8_t ep_address) {
  return xhci_reset_endpoint(xhci_.get(), device_id, ep_address);
}

zx_status_t UsbXhci::UsbHciResetDevice(uint32_t hub_address, uint32_t device_id) {
  auto* xhci = xhci_.get();
  auto* slot = &xhci->slots[device_id];
  uint32_t port = slot->port;
  if (slot->hub_address == 0) {
    // Convert real port number to virtual root hub number.
    port = xhci->rh_port_map[port - 1] + 1;
  }
  zxlogf(DEBUG, "xhci_reset_device slot_id: %u port: %u hub_address: %u", device_id, port,
         hub_address);

  return usb_bus_interface_reset_port(&xhci->bus, hub_address, port, false);
}

static size_t xhci_get_max_transfer_size(uint8_t ep_address) {
  if (ep_address == 0) {
    // control requests have uint16 length field so we need to support UINT16_MAX
    // we require one setup, status and data event TRB in addition to data transfer TRBs
    // and subtract one more to account for the link TRB
    assert(zx_system_get_page_size() * (TRANSFER_RING_SIZE - 4) >= UINT16_MAX);
    return UINT16_MAX;
  }
  // non-control transfers consist of normal transfer TRBs plus one data event TRB
  // Subtract 2 to reserve a TRB for data event and to account for the link TRB
  return zx_system_get_page_size() * (TRANSFER_RING_SIZE - 2);
}

size_t UsbXhci::UsbHciGetMaxTransferSize(uint32_t device_id, uint8_t ep_address) {
  return xhci_get_max_transfer_size(ep_address);
}

zx_status_t UsbXhci::UsbHciCancelAll(uint32_t device_id, uint8_t ep_address) {
  return xhci_cancel_transfers(xhci_.get(), device_id, xhci_endpoint_index(ep_address));
}

size_t UsbXhci::UsbHciGetRequestSize() {
  return sizeof(xhci_usb_request_internal_t) + sizeof(usb_request_t);
}

void xhci_request_queue(xhci_t* xhci, usb_request_t* req,
                        const usb_request_complete_callback_t* complete_cb) {
  zx_status_t status;

  xhci_usb_request_internal_t* req_int = USB_REQ_TO_XHCI_INTERNAL(req);
  req_int->complete_cb = *complete_cb;
  if (req->header.length > xhci_get_max_transfer_size(req->header.ep_address)) {
    status = ZX_ERR_INVALID_ARGS;
  } else {
    status = xhci_queue_transfer(xhci, req);
  }

  if (status != ZX_OK && status != ZX_ERR_BUFFER_TOO_SMALL) {
    usb_request_complete(req, status, 0, complete_cb);
  }
}

static void xhci_shutdown(xhci_t* xhci) {
  // stop the controller and our device thread
  xhci_stop(xhci);
  xhci->suspended.store(true);
  // stop our interrupt threads
  for (uint32_t i = 0; i < xhci->num_interrupts; i++) {
    xhci->irq_handles[i].destroy();
    thrd_join(xhci->completer_threads[i], nullptr);
  }
}

void UsbXhci::DdkSuspend(ddk::SuspendTxn txn) {
  // TODO(fxbug.dev/42612): do different things based on the requested_state and suspend reason.
  // for now we shutdown the driver in preparation for mexec
  xhci_shutdown(xhci_.get());
  txn.Reply(ZX_OK, 0);
}

void UsbXhci::DdkUnbind(ddk::UnbindTxn txn) {
  zxlogf(INFO, "UsbXhci::DdkUnbind");
  if (init_success_) {
    xhci_shutdown(xhci_.get());
  }
  txn.Reply();
}

void UsbXhci::DdkRelease() {
  zxlogf(INFO, "UsbXhci::DdkRelease");
  delete this;
}

int UsbXhci::CompleterThread(void* arg) {
  auto* completer = static_cast<Completer*>(arg);
  auto* xhci = completer->xhci;
  auto interrupter = completer->interrupter;
  auto& interrupt = xhci->irq_handles[interrupter];

  // TODO(johngro): See fxbug.dev/30888.  Get rid of this.  For now we need thread
  // priorities so that realtime transactions use the completer which ends
  // up getting realtime latency guarantees.
  if (completer->high_priority) {
    if (xhci->profile_handle.is_valid()) {
      zx_object_set_profile(zx_thread_self(), xhci->profile_handle.get(), 0);
    } else {
      zxlogf(WARNING,
             "No scheduler profile available to apply to the high priority XHCI completer.  "
             "Service will be best effort.\n");
    }
  }

  while (1) {
    zx_status_t wait_res;
    wait_res = interrupt.wait(nullptr);
    if (wait_res != ZX_OK) {
      if (wait_res != ZX_ERR_CANCELED) {
        zxlogf(ERROR, "unexpected zx_interrupt_wait failure (%d)", wait_res);
      }
      break;
    }
    if (xhci->suspended.load()) {
      // TODO(ravoorir): Remove this hack once the interrupt signalling bug
      // is resolved.
      zxlogf(ERROR, "race in zx_interrupt_cancel triggered. Kick off workaround for now");
      break;
    }
    xhci_handle_interrupt(xhci, interrupter);
  }
  zxlogf(DEBUG, "xhci completer %u thread done", interrupter);
  return 0;
}

int UsbXhci::StartThread() {
  zxlogf(DEBUG, "%s start", __func__);

  assert(init_txn_.has_value());

  fbl::AllocChecker ac;
  completers_.reset(new (&ac) Completer[xhci_->num_interrupts], xhci_->num_interrupts);
  if (!ac.check()) {
    init_txn_->Reply(ZX_ERR_NO_MEMORY);
    return ZX_ERR_NO_MEMORY;
  }

  for (uint32_t i = 0; i < xhci_->num_interrupts; i++) {
    auto* completer = &completers_[i];
    completer->xhci = xhci_.get();
    completer->interrupter = i;
    // We need a high priority thread for isochronous transfers.
    // If there is only one interrupt available, that thread will need
    // to be high priority.
    completer->high_priority = i == ISOCH_INTERRUPTER || xhci_->num_interrupts == 1;
  }

  // xhci_start will block, so do this part here instead of in usb_xhci_bind
  auto status = xhci_start(xhci_.get());
  if (status != ZX_OK) {
    init_txn_->Reply(status);
    return status;
  }

  init_success_ = true;
  // This will make the device visible and able to be unbound.
  init_txn_->Reply(ZX_OK);
  for (uint32_t i = 0; i < xhci_->num_interrupts; i++) {
    thrd_create_with_name(&xhci_->completer_threads[i], CompleterThread, &completers_[i],
                          "xhci_completer_thread");
  }

  zxlogf(DEBUG, "%s done", __func__);
  return 0;
}

zx_status_t UsbXhci::FinishBind() { return DdkAdd("xhci"); }

void UsbXhci::DdkInit(ddk::InitTxn txn) {
  // Configure and fetch a deadline profile for the high priority USB completer
  // thread.  In a case where we are taking an interrupt on every microframe, we
  // will need to run at 8KHz and have reserved up to 66% of a CPU for work in
  // that period.
  zx_status_t status = device_get_deadline_profile(
      zxdev_,
      ZX_USEC(80),   // capacity: we agree to run for no more than 80 uSec max
      ZX_USEC(120),  // deadline: we need to be done before the next microframe (125 uSec)
      ZX_USEC(120),  // period:   Worst case period is one IRQ per microframe (8KHz)
      "src/devices/usb/drivers/xhci/usb-xhci", xhci_->profile_handle.reset_and_get_address());

  if (status != ZX_OK) {
    zxlogf(INFO, "Failed to obtain scheduler profile for high priority completer (res %d)", status);
  }

  // The StartThread will reply to |init_txn_|.
  init_txn_ = std::move(txn);

  thrd_t thread;
  thrd_create_with_name(
      &thread, [](void* arg) -> int { return reinterpret_cast<UsbXhci*>(arg)->StartThread(); },
      reinterpret_cast<void*>(this), "xhci_start_thread");
  thrd_detach(thread);
}

zx_status_t UsbXhci::InitPci() {
  zx_status_t status;

  if (!pci_.is_valid()) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  fbl::AllocChecker ac;
  xhci_ = std::unique_ptr<xhci_t>(new (&ac) xhci_t);
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }

  status = pci_.GetBti(0, &xhci_->bti_handle);
  if (status != ZX_OK) {
    return status;
  }

  // eXtensible Host Controller Interface revision 1.1, section 5, xhci
  // should only use BARs 0 and 1. 0 for 32 bit addressing, and 0+1 for 64 bit addressing.

  // TODO(voydanoff) find a C++ way to do this
  pci_protocol_t pci;
  pci_.GetProto(&pci);
  mmio_buffer_t mmio;
  status = pci_map_bar_buffer(&pci, 0u, ZX_CACHE_POLICY_UNCACHED, &mmio);
  if (status != ZX_OK) {
    zxlogf(ERROR, "%s could not map bar", __func__);
    return status;
  }
  xhci_->mmio = ddk::MmioBuffer(mmio);

  // Cap IRQ count at the number of interrupters we want to use and
  // the number of interrupters supported by XHCI.
  uint32_t irq_cnt = std::min(INTERRUPTER_COUNT, xhci_get_max_interrupters(xhci_.get()));
  while (irq_cnt && (status = pci_.ConfigureIrqMode(irq_cnt, nullptr)) != ZX_OK) {
    irq_cnt--;
  }

  if (!irq_cnt || status != ZX_OK) {
    zxlogf(ERROR, "usb_xhci_bind Failed to set IRQ mode (err = %d)", status);
    return status;
  }

  for (uint32_t i = 0; i < irq_cnt; i++) {
    // register for interrupts
    status = pci_.MapInterrupt(i, &xhci_->irq_handles[i]);
    if (status != ZX_OK) {
      zxlogf(ERROR, "usb_xhci_bind map_interrupt failed %d", status);
      return status;
    }
  }

  // used for enabling bus mastering
  pci_.GetProto(&xhci_->pci);

  status = xhci_init(xhci_.get(), XHCI_PCI, irq_cnt);
  if (status != ZX_OK) {
    return status;
  }
  status = FinishBind();
  if (status != ZX_OK) {
    return status;
  }

  return ZX_OK;
}

zx_status_t UsbXhci::InitPdev() {
  zx_status_t status;

  fbl::AllocChecker ac;
  xhci_ = std::unique_ptr<xhci_t>(new (&ac) xhci_t);
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }

  status = pdev_.GetBti(0, &xhci_->bti_handle);
  if (status != ZX_OK) {
    return status;
  }

  // TODO(voydanoff) find a C++ way to do this
  status = pdev_.MapMmio(PDEV_MMIO_INDEX, &xhci_->mmio);
  if (status != ZX_OK) {
    zxlogf(ERROR, "%s: pdev_map_mmio failed", __func__);
    return status;
  }

  status = pdev_.GetInterrupt(PDEV_IRQ_INDEX, &xhci_->irq_handles[0]);
  if (status != ZX_OK) {
    zxlogf(ERROR, "%s: pdev_map_interrupt failed", __func__);
    return status;
  }

  status = xhci_init(xhci_.get(), XHCI_PDEV, 1);
  if (status != ZX_OK) {
    return status;
  }
  status = FinishBind();
  if (status != ZX_OK) {
    return status;
  }

  return ZX_OK;
}

zx_status_t UsbXhci::Init() {
  if (pci_.is_valid()) {
    return InitPci();
  } else if (pdev_.is_valid()) {
    return InitPdev();
  } else if (DdkGetFragmentCount() > 0) {
    pdev_ = ddk::PDev::FromFragment(parent());
    if (!pdev_.is_valid()) {
      zxlogf(ERROR, "UsbXhci::Init: could not get platform device protocol");
      return ZX_ERR_NOT_SUPPORTED;
    }

    return InitPdev();
  } else {
    return ZX_ERR_NOT_SUPPORTED;
  }
}

zx_status_t UsbXhci::Create(void* ctx, zx_device_t* parent) {
  fbl::AllocChecker ac;
  auto dev = std::unique_ptr<UsbXhci>(new (&ac) UsbXhci(parent));
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }

  auto status = dev->Init();
  if (status != ZX_OK) {
    return status;
  }

  // devmgr is now in charge of the device.
  __UNUSED auto* dummy = dev.release();
  return ZX_OK;
}

static constexpr zx_driver_ops_t driver_ops = []() {
  zx_driver_ops_t ops = {};
  ops.version = DRIVER_OPS_VERSION;
  ops.bind = UsbXhci::Create;
  return ops;
}();

}  // namespace usb_xhci

ZIRCON_DRIVER(usb_xhci, usb_xhci::driver_ops, "zircon", "0.1");
