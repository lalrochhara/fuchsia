// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/graphics/drivers/misc/goldfish_sync/sync_device.h"

#include <fuchsia/hardware/goldfish/llcpp/fidl.h>
#include <lib/async-loop/loop.h>
#include <lib/async/cpp/task.h>
#include <lib/async/cpp/wait.h>
#include <lib/async/dispatcher.h>
#include <lib/ddk/debug.h>
#include <lib/ddk/trace/event.h>
#include <threads.h>
#include <zircon/assert.h>
#include <zircon/errors.h>
#include <zircon/threads.h>

#include <iterator>

#include <ddktl/fidl.h>
#include <fbl/auto_lock.h>

#include "src/graphics/drivers/misc/goldfish_sync/goldfish_sync-bind.h"
#include "src/graphics/drivers/misc/goldfish_sync/sync_common_defs.h"

namespace goldfish {

namespace sync {

namespace {

// This value is passed to bti_create as a marker; it does not have a particular
// meaning to anything in the system.
constexpr uint32_t GOLDFISH_SYNC_BTI_ID = 0x80888099;

uint32_t upper_32_bits(uint64_t n) { return static_cast<uint32_t>(n >> 32); }

uint32_t lower_32_bits(uint64_t n) { return static_cast<uint32_t>(n); }

}  // namespace

// static
zx_status_t SyncDevice::Create(void* ctx, zx_device_t* device) {
  auto sync_device =
      std::make_unique<goldfish::sync::SyncDevice>(device, /* can_read_multiple_commands= */ true);

  zx_status_t status = sync_device->Bind();
  if (status == ZX_OK) {
    // devmgr now owns device.
    __UNUSED auto* dev = sync_device.release();
  }
  return status;
}

SyncDevice::SyncDevice(zx_device_t* parent, bool can_read_multiple_commands)
    : SyncDeviceType(parent),
      can_read_multiple_commands_(can_read_multiple_commands),
      acpi_(parent),
      loop_(&kAsyncLoopConfigNeverAttachToThread) {
  loop_.StartThread("goldfish-sync-loop-thread");
}

SyncDevice::~SyncDevice() {
  if (irq_.is_valid()) {
    irq_.destroy();
  }
  if (irq_thread_.has_value()) {
    thrd_join(irq_thread_.value(), nullptr);
  }
  loop_.Shutdown();
}

zx_status_t SyncDevice::Bind() {
  if (!acpi_.is_valid()) {
    zxlogf(ERROR, "no acpi protocol");
    return ZX_ERR_NOT_SUPPORTED;
  }

  zx_status_t status = acpi_.GetBti(GOLDFISH_SYNC_BTI_ID, 0, &bti_);
  if (status != ZX_OK) {
    zxlogf(ERROR, "GetBti failed: %d", status);
    return status;
  }

  acpi_mmio_t mmio;
  status = acpi_.GetMmio(0, &mmio);
  if (status != ZX_OK) {
    zxlogf(ERROR, "GetMmio failed: %d", status);
    return status;
  }

  {
    fbl::AutoLock lock(&mmio_lock_);
    status = ddk::MmioBuffer::Create(mmio.offset, mmio.size, zx::vmo(mmio.vmo),
                                     ZX_CACHE_POLICY_UNCACHED_DEVICE, &mmio_);
    if (status != ZX_OK) {
      zxlogf(ERROR, "mmiobuffer create failed: %d", status);
      return status;
    }
  }

  status = acpi_.MapInterrupt(0, &irq_);
  if (status != ZX_OK) {
    zxlogf(ERROR, "map_interrupt failed: %d", status);
    return status;
  }

  irq_thread_.emplace(thrd_t{});
  int rc = thrd_create_with_name(
      &irq_thread_.value(), [](void* arg) { return static_cast<SyncDevice*>(arg)->IrqHandler(); },
      this, "goldfish_sync_irq_thread");
  if (rc != thrd_success) {
    irq_.destroy();
    return thrd_status_to_zx_status(rc);
  }

  fbl::AutoLock cmd_lock(&cmd_lock_);
  fbl::AutoLock mmio_lock(&mmio_lock_);
  static_assert(sizeof(CommandBuffers) <= PAGE_SIZE, "cmds size");
  status = io_buffer_.Init(bti_.get(), PAGE_SIZE, IO_BUFFER_RW | IO_BUFFER_CONTIG);
  if (status != ZX_OK) {
    zxlogf(ERROR, "io_buffer_init failed: %d", status);
    return status;
  }

  // Register the buffer addresses with the device.
  // Device requires the lower 32 bits to be sent first for each address.
  zx_paddr_t pa_batch_hostcmd = io_buffer_.phys() + offsetof(CommandBuffers, batch_hostcmd);
  mmio_->Write32(lower_32_bits(pa_batch_hostcmd), SYNC_REG_BATCH_COMMAND_ADDR);
  mmio_->Write32(upper_32_bits(pa_batch_hostcmd), SYNC_REG_BATCH_COMMAND_ADDR_HIGH);

  ZX_DEBUG_ASSERT(lower_32_bits(pa_batch_hostcmd) == mmio_->Read32(SYNC_REG_BATCH_COMMAND_ADDR));
  ZX_DEBUG_ASSERT(upper_32_bits(pa_batch_hostcmd) ==
                  mmio_->Read32(SYNC_REG_BATCH_COMMAND_ADDR_HIGH));

  zx_paddr_t pa_batch_guestcmd = io_buffer_.phys() + offsetof(CommandBuffers, batch_guestcmd);
  mmio_->Write32(lower_32_bits(pa_batch_guestcmd), SYNC_REG_BATCH_GUESTCOMMAND_ADDR);
  mmio_->Write32(upper_32_bits(pa_batch_guestcmd), SYNC_REG_BATCH_GUESTCOMMAND_ADDR_HIGH);

  ZX_DEBUG_ASSERT(lower_32_bits(pa_batch_guestcmd) ==
                  mmio_->Read32(SYNC_REG_BATCH_GUESTCOMMAND_ADDR));
  ZX_DEBUG_ASSERT(upper_32_bits(pa_batch_guestcmd) ==
                  mmio_->Read32(SYNC_REG_BATCH_GUESTCOMMAND_ADDR_HIGH));

  mmio_->Write32(0, SYNC_REG_INIT);

  return DdkAdd(ddk::DeviceAddArgs("goldfish-sync").set_proto_id(ZX_PROTOCOL_GOLDFISH_SYNC));
}

void SyncDevice::DdkUnbind(ddk::UnbindTxn txn) { txn.Reply(); }

void SyncDevice::DdkRelease() { delete this; }

zx_status_t SyncDevice::GoldfishSyncCreateTimeline(zx::channel request) {
  fbl::RefPtr<SyncTimeline> timeline = fbl::MakeRefCounted<SyncTimeline>(this);
  timelines_.push_back(timeline);
  zx_status_t status = timeline->Bind(std::move(request));
  if (status != ZX_OK) {
    zxlogf(ERROR, "CreateTimeline: Cannot bind timeline, status = %d", status);
    timeline->RemoveFromContainer();
  }
  return status;
}

void SyncDevice::CreateTimeline(CreateTimelineRequestView request,
                                CreateTimelineCompleter::Sync& completer) {
  fbl::RefPtr<SyncTimeline> timeline = fbl::MakeRefCounted<SyncTimeline>(this);
  timelines_.push_back(timeline);
  timeline->Bind(request->timeline_req.TakeChannel());
  completer.Reply();
}

bool SyncDevice::ReadCommands() {
  fbl::AutoLock cmd_lock(&cmd_lock_);
  fbl::AutoLock mmio_lock(&mmio_lock_);
  bool staged_commands_was_empty = staged_commands_.empty();
  while (true) {
    mmio_->Read32(SYNC_REG_BATCH_COMMAND);
    auto cmd_bufs = reinterpret_cast<CommandBuffers*>(io_buffer_.virt());
    if (cmd_bufs->batch_hostcmd.cmd == 0) {
      // no more new commands
      break;
    }

    staged_commands_.push_back(cmd_bufs->batch_hostcmd);
    if (!can_read_multiple_commands_) {
      break;
    }
  }
  return staged_commands_was_empty && !staged_commands_.empty();
}

void SyncDevice::RunHostCommand(HostCommand command) {
  switch (command.cmd) {
    case CMD_SYNC_READY: {
      TRACE_DURATION("gfx", "Sync::HostCommand::Ready");
      break;
    }
    case CMD_CREATE_SYNC_FENCE: {
      TRACE_DURATION("gfx", "Sync::HostCommand::CreateSyncFence", "timeline", command.handle,
                     "hostcmd_handle", command.hostcmd_handle);
      SyncTimeline* timeline = reinterpret_cast<SyncTimeline*>(command.handle);
      ZX_DEBUG_ASSERT(timeline);

      zx::eventpair event_device, event_client;
      zx_status_t status = zx::eventpair::create(0u, &event_device, &event_client);
      ZX_DEBUG_ASSERT(status == ZX_OK);

      timeline->CreateFence(std::move(event_device), command.time_arg);
      ReplyHostCommand({
          .handle = event_client.release(),
          .hostcmd_handle = command.hostcmd_handle,
          .cmd = command.cmd,
          .time_arg = 0,
      });
      break;
    }
    case CMD_CREATE_SYNC_TIMELINE: {
      TRACE_DURATION("gfx", "Sync::HostCommand::CreateTimeline", "hostcmd_handle",
                     command.hostcmd_handle);
      fbl::RefPtr<SyncTimeline> timeline = fbl::MakeRefCounted<SyncTimeline>(this);
      timelines_.push_back(timeline);
      ReplyHostCommand({
          .handle = reinterpret_cast<uint64_t>(timeline.get()),
          .hostcmd_handle = command.hostcmd_handle,
          .cmd = command.cmd,
          .time_arg = 0,
      });
      break;
    }
    case CMD_SYNC_TIMELINE_INC: {
      TRACE_DURATION("gfx", "Sync::HostCommand::TimelineInc", "timeline", command.handle,
                     "time_arg", command.time_arg);
      SyncTimeline* timeline = reinterpret_cast<SyncTimeline*>(command.handle);
      ZX_DEBUG_ASSERT(timeline);
      timeline->Increase(command.time_arg);
      break;
    }
    case CMD_DESTROY_SYNC_TIMELINE: {
      TRACE_DURATION("gfx", "Sync::HostCommand::DestroySyncTimeline", "timeline", command.handle);
      SyncTimeline* timeline = reinterpret_cast<SyncTimeline*>(command.handle);
      ZX_DEBUG_ASSERT(timeline);
      ZX_DEBUG_ASSERT(timeline->InContainer());
      timeline->RemoveFromContainer();
      break;
    }
  }
}

void SyncDevice::ReplyHostCommand(HostCommand command) {
  fbl::AutoLock cmd_lock(&cmd_lock_);
  auto cmd_bufs = reinterpret_cast<CommandBuffers*>(io_buffer_.virt());
  memcpy(&cmd_bufs->batch_hostcmd, &command, sizeof(HostCommand));

  fbl::AutoLock mmio_lock(&mmio_lock_);
  mmio_->Write32(0, SYNC_REG_BATCH_COMMAND);
}

void SyncDevice::SendGuestCommand(GuestCommand command) {
  fbl::AutoLock cmd_lock(&cmd_lock_);
  auto cmd_bufs = reinterpret_cast<CommandBuffers*>(io_buffer_.virt());
  memcpy(&cmd_bufs->batch_guestcmd, &command, sizeof(GuestCommand));

  fbl::AutoLock mmio_lock(&mmio_lock_);
  mmio_->Write32(0, SYNC_REG_BATCH_GUESTCOMMAND);
}

void SyncDevice::HandleStagedCommands() {
  std::list<HostCommand> commands;

  {
    fbl::AutoLock cmd_lock(&cmd_lock_);
    commands.splice(commands.begin(), staged_commands_, staged_commands_.begin(),
                    staged_commands_.end());
    ZX_DEBUG_ASSERT(staged_commands_.empty());
  }

  for (const auto& command : commands) {
    RunHostCommand(command);
  }
}

int SyncDevice::IrqHandler() {
  while (true) {
    zx_status_t status = irq_.wait(nullptr);
    if (status != ZX_OK) {
      // ZX_ERR_CANCELED means the ACPI irq is cancelled, and the interrupt
      // thread should exit normally.
      if (status != ZX_ERR_CANCELED) {
        zxlogf(ERROR, "irq.wait() got %d", status);
      }
      break;
    }

    // Handle incoming commands.
    if (ReadCommands()) {
      async::PostTask(loop_.dispatcher(), [this]() { HandleStagedCommands(); });
    }
  }

  return 0;
}

SyncTimeline::SyncTimeline(SyncDevice* parent)
    : parent_device_(parent), dispatcher_(parent->loop()->dispatcher()) {}

SyncTimeline::~SyncTimeline() = default;

zx_status_t SyncTimeline::Bind(zx::channel request) {
  zx_handle_t server_handle = request.release();
  return async::PostTask(dispatcher_, [server_handle, this]() mutable {
    using SyncTimelineProtocol = fuchsia_hardware_goldfish::SyncTimeline;
    fidl::BindServer(dispatcher_, zx::channel(server_handle), this,
                     [](SyncTimeline* self, fidl::UnbindInfo info,
                        fidl::ServerEnd<SyncTimelineProtocol> server_end) {
                       self->OnClose(info, server_end.TakeChannel());
                     });
  });
  return ZX_OK;
}

void SyncTimeline::OnClose(fidl::UnbindInfo info, zx::channel channel) {
  if (info.reason() == fidl::Reason::kPeerClosed) {
    zxlogf(INFO, "Client closed SyncTimeline connection: epitaph: %d", info.status());
  } else if (!info.ok()) {
    zxlogf(ERROR, "Channel internal error: %s", info.FormatDescription().c_str());
  }

  if (InContainer()) {
    RemoveFromContainer();
  }
}

void SyncTimeline::TriggerHostWait(TriggerHostWaitRequestView request,
                                   TriggerHostWaitCompleter::Sync& completer) {
  TRACE_DURATION("gfx", "Sync::GuestCommand::TriggerHostWait", "timeline", this, "glsync",
                 request->host_glsync_handle, "syncthread", request->host_syncthread_handle);
  CreateFence(std::move(request->event));
  parent_device_->SendGuestCommand({.host_command = CMD_TRIGGER_HOST_WAIT,
                                    .glsync_handle = request->host_glsync_handle,
                                    .thread_handle = request->host_syncthread_handle,
                                    .guest_timeline_handle = reinterpret_cast<uint64_t>(this)});
}

void SyncTimeline::Increase(uint64_t step) {
  TRACE_DURATION("gfx", "SyncTimeline::Increase", "timeline", this, "step", step);
  fbl::AutoLock lock(&lock_);

  seqno_ += step;
  while (!active_fences_.is_empty()) {
    if (seqno_ < active_fences_.front().seqno) {
      break;
    }
    auto fence = active_fences_.pop_front();
    fence->event.signal_peer(0u, ZX_EVENTPAIR_SIGNALED);
    inactive_fences_.push_back(std::move(fence));
  }
}

void SyncTimeline::CreateFence(zx::eventpair event, std::optional<uint64_t> seqno) {
  TRACE_DURATION("gfx", "SyncTimeline::CreateFence", "timeline", this);

  std::unique_ptr<Fence> fence = std::make_unique<Fence>();
  Fence* fence_ptr = fence.get();
  {
    fbl::AutoLock lock(&lock_);

    fence->timeline = fbl::RefPtr(this);
    fence->event = std::move(event);
    fence->seqno = seqno.value_or(seqno_ + 1);
    // If the event's peer sent to the clients is closed, we can safely remove the
    // fence.
    fence->peer_closed_wait = std::make_unique<async::Wait>(
        fence_ptr->event.get(), ZX_EVENTPAIR_PEER_CLOSED, 0u,
        // We keep the RefPtr of |this| so that we can ensure |timeline| is
        // always valid in the callback, otherwise when the last fence is
        // removed from the container, it will destroy the sync timeline and
        // cause a use-after-free error.
        [fence = fence_ptr, timeline = fbl::RefPtr(this)](async_dispatcher_t* dispatcher,
                                                          async::Wait* wait, zx_status_t status,
                                                          const zx_packet_signal_t* signal) {
          if (signal == nullptr || (signal->observed & ZX_EVENTPAIR_PEER_CLOSED)) {
            if (status != ZX_OK && status != ZX_ERR_CANCELED) {
              zxlogf(ERROR, "CreateFence: Unexpected Wait status: %d", status);
            }
            fbl::AutoLock lock(&timeline->lock_);
            ZX_DEBUG_ASSERT(fence->InContainer());
            fence->RemoveFromContainer();
          }
        });

    if (seqno_ >= fence->seqno) {
      // Fence is inactive. Store it in |inactive_fences_| list until its
      // peer disconnects.
      inactive_fences_.push_back(std::move(fence));
    } else {
      // Maintain the seqno order in the active fences linked list.
      auto iter = active_fences_.end();
      while (iter != active_fences_.begin()) {
        if ((--iter)->seqno < fence->seqno) {
          ++iter;
          break;
        }
      }
      active_fences_.insert(iter, std::move(fence));
    }
  }

  fence_ptr->peer_closed_wait->Begin(dispatcher_);
}

}  // namespace sync

}  // namespace goldfish

static constexpr zx_driver_ops_t goldfish_sync_driver_ops = []() -> zx_driver_ops_t {
  zx_driver_ops_t ops = {};
  ops.version = DRIVER_OPS_VERSION;
  ops.bind = goldfish::sync::SyncDevice::Create;
  return ops;
}();

ZIRCON_DRIVER(goldfish_sync, goldfish_sync_driver_ops, "zircon", "0.1");
